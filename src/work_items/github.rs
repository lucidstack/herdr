//! GitHub pull requests read through the GitHub CLI: review requests for you, and your own
//! pull requests with changes requested.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};
use crate::config::{
    BranchWorkflowConfig, GithubRepoConfig, GithubWorkItemsConfig, OnResolvedConfig,
    ReviewRequestedConfig,
};

use super::process::{failure_detail, run_with_timeout};
use super::source::{
    DownloadSpec, ItemChoices, PreparedItem, ProvisionPlan, SourceItem, WorkItemSource,
    WorkspaceLayout, WorkspaceSource, WorktreeSpec,
};
use super::state::WorkItem;

const SOURCE_ID: &str = "github";
const GH_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_POLL_SECONDS: u64 = 30;
const MAX_POLL_SECONDS: u64 = 3600;
/// GitHub's search page size and per-query result limits.
const SEARCH_PAGE_SIZE: usize = 100;
const MAX_SEARCH_RESULTS: usize = 1000;
const GITHUB_CHOICE_ID: &str = "github";
const MAX_WORKSPACE_LABEL_CHARS: usize = 40;
const MAX_BRIEF_BODY_CHARS: usize = 4000;
const MAX_BRIEF_FILES: usize = 100;
const MAX_BRIEF_COMMENTS: usize = 50;
const MAX_COMMENT_CHARS: usize = 600;
/// External-id prefixes per event. Review requests carry none, which keeps the ids of items
/// stored before other events existed.
const CHANGES_PREFIX: &str = "changes:";
const CI_PREFIX: &str = "ci:";
const ASSIGNED_PREFIX: &str = "assigned:";
const MENTION_PREFIX: &str = "mention:";
const MERGE_PREFIX: &str = "merge:";

/// What a GitHub item asks of you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// Someone requested your review.
    ReviewRequested,
    /// A reviewer requested changes on your pull request.
    ChangesRequested,
    /// Checks are failing on your pull request.
    CiFailing,
    /// An issue is assigned to you.
    Assigned,
    /// An issue or pull request mentions you.
    Mentioned,
    /// Your pull request is approved and waits for you to merge it.
    ReadyToMerge,
}

impl Event {
    const ALL: [Self; 6] = [
        Self::ReviewRequested,
        Self::ChangesRequested,
        Self::CiFailing,
        Self::Assigned,
        Self::Mentioned,
        Self::ReadyToMerge,
    ];

    fn of(external_id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|event| {
                !event.id_prefix().is_empty() && external_id.starts_with(event.id_prefix())
            })
            .unwrap_or(Self::ReviewRequested)
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::ReviewRequested => "",
            Self::ChangesRequested => CHANGES_PREFIX,
            Self::CiFailing => CI_PREFIX,
            Self::Assigned => ASSIGNED_PREFIX,
            Self::Mentioned => MENTION_PREFIX,
            Self::ReadyToMerge => MERGE_PREFIX,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review requests",
            Self::ChangesRequested => "changes requested",
            Self::CiFailing => "failing checks",
            Self::Assigned => "assigned issues",
            Self::Mentioned => "mentions",
            Self::ReadyToMerge => "ready to merge",
        }
    }

    /// Short tag shown after the number in the sidebar; empty for review requests.
    fn tag(self) -> &'static str {
        match self {
            Self::ReviewRequested => "",
            Self::ChangesRequested => "changes",
            Self::CiFailing => "ci failing",
            Self::Assigned => "assigned",
            Self::Mentioned => "mention",
            Self::ReadyToMerge => "ready to merge",
        }
    }

    fn arrival_title(self) -> &'static str {
        match self {
            Self::ReviewRequested => "Review requested",
            Self::ChangesRequested => "Changes requested",
            Self::CiFailing => "Checks failing",
            Self::Assigned => "Issue assigned",
            Self::Mentioned => "You were mentioned",
            Self::ReadyToMerge => "Ready to merge",
        }
    }

    /// Items are pull requests whose details come from `gh pr view`.
    fn is_pull_request_event(self) -> bool {
        matches!(
            self,
            Self::ReviewRequested | Self::ChangesRequested | Self::CiFailing | Self::ReadyToMerge
        )
    }

    fn modes(self) -> &'static [ReviewMode] {
        match self {
            Self::ReviewRequested => &[
                ReviewMode::Local,
                ReviewMode::LocalAgentReview,
                ReviewMode::AgentReport,
                ReviewMode::AgentPost,
            ],
            Self::ChangesRequested => &[ReviewMode::Address, ReviewMode::AddressAgent],
            Self::CiFailing => &[ReviewMode::FixChecks, ReviewMode::FixChecksAgent],
            Self::Assigned => &[ReviewMode::StartIssue, ReviewMode::StartIssueAgent],
            Self::Mentioned => &[ReviewMode::ThreadAgent],
            // Merging is carried out by the source, not in a workspace.
            Self::ReadyToMerge => &[],
        }
    }
}

pub(crate) struct GithubSource {
    config: GithubWorkItemsConfig,
    /// Review-request blocks with compiled patterns, in configuration order.
    review_requested: Vec<Workflow>,
    /// Defaults used when no block matches a repository.
    fallback: Workflow,
    branch_fallback: BranchWorkflowConfig,
    build_error: Option<String>,
}

/// One `[[work_items.github.review_requested]]` block, ready for matching.
struct Workflow {
    config: ReviewRequestedConfig,
    docs: Vec<Regex>,
}

/// The workflow settings that apply to one item.
struct Settings<'a> {
    agent: &'a str,
    agent_args: &'a [String],
    editor_command: &'a str,
    lazygit_command: &'a str,
    /// Viewer of the downloaded file when there is no checkout; `{file}` is the file.
    diff_command: &'a str,
    delete_branch: bool,
    on_resolved: OnResolvedConfig,
}

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    number: u64,
    title: String,
    html_url: String,
    updated_at: String,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    user: Option<SearchUser>,
    #[serde(default)]
    repository_url: Option<String>,
}

#[derive(Deserialize)]
struct SearchUser {
    login: String,
}

/// Pull request details kept as the item's source-opaque detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubDetail {
    pub number: u64,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub changed_files: u64,
    #[serde(default)]
    pub files: Vec<GithubFile>,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    /// The head branch lives in a fork, so the configured remote does not serve it.
    #[serde(default)]
    pub is_cross_repository: bool,
    /// Submitted reviews; only fetched for changes-requested items.
    #[serde(default)]
    pub reviews: Vec<GithubReview>,
    /// Inline review comments; only fetched for changes-requested items.
    #[serde(default)]
    pub inline_comments: Vec<GithubInlineComment>,
    /// Failing checks; only fetched for failing-checks items.
    #[serde(default)]
    pub failing_checks: Vec<GithubCheck>,
    /// GitHub's merge readiness (CLEAN, BEHIND, DIRTY, BLOCKED…); ready-to-merge items only.
    #[serde(default)]
    pub merge_state_status: String,
    /// Each reviewer's latest review; ready-to-merge items only.
    #[serde(default)]
    pub latest_reviews: Vec<GithubReview>,
    /// Merge methods the repository allows (MERGE, SQUASH, REBASE), your default first.
    #[serde(default)]
    pub merge_methods: Vec<String>,
}

/// One entry of `gh pr checks --json name,state,bucket,link,workflow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubCheck {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub workflow: String,
}

/// Issue (or pull request, for mentions) details kept as the item's detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubIssueDetail {
    pub number: u64,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub is_pull_request: bool,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Oldest first.
    #[serde(default)]
    pub comments: Vec<GithubIssueComment>,
    /// Default branch of the repository; fetched for assigned issues.
    #[serde(default)]
    pub default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubIssueComment {
    pub author: String,
    pub body: String,
}

/// `GET repos/{repo}/issues/{n}`.
#[derive(Deserialize)]
struct RestIssue {
    number: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<RestLabel>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RestLabel {
    name: String,
}

/// `GET repos/{repo}/issues/{n}/comments`.
#[derive(Deserialize)]
struct RestIssueComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    user: Option<SearchUser>,
}

#[derive(Deserialize)]
struct RestRepository {
    default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubLogin {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubReview {
    #[serde(default)]
    pub author: Option<GithubLogin>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubInlineComment {
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub author: String,
    pub body: String,
}

/// One entry of `GET repos/{repo}/pulls/{n}/comments`.
#[derive(Deserialize)]
struct RestReviewComment {
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    user: Option<SearchUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubFile {
    pub path: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

/// `owner/repo#123`, optionally behind the event prefix, split into repository and number.
fn parse_external_id(external_id: &str) -> Option<(&str, u64)> {
    let id = external_id
        .strip_prefix(Event::of(external_id).id_prefix())
        .unwrap_or(external_id);
    let (repo, number) = id.rsplit_once('#')?;
    Some((repo, number.parse().ok()?))
}

fn compile_patterns(patterns: &[String], key: &str) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|err| format!("invalid {key} pattern {pattern:?}: {err}"))
        })
        .collect()
}

impl GithubSource {
    pub(crate) fn new(config: GithubWorkItemsConfig) -> Self {
        let mut build_error = None;
        let mut compile = |block: &ReviewRequestedConfig| Workflow {
            config: block.clone(),
            docs: compile_patterns(&block.docs_patterns, "docs_patterns").unwrap_or_else(|err| {
                build_error.get_or_insert(err);
                Vec::new()
            }),
        };
        let review_requested = config.review_requested.iter().map(&mut compile).collect();
        let fallback = compile(&ReviewRequestedConfig::default());
        Self {
            config,
            review_requested,
            fallback,
            branch_fallback: BranchWorkflowConfig::default(),
            build_error,
        }
    }

    /// The review-request workflow for `repo`: the first matching block, else the defaults.
    fn workflow(&self, repo: &str) -> &Workflow {
        self.review_requested
            .iter()
            .find(|workflow| workflow.config.applies_to(repo))
            .unwrap_or(&self.fallback)
    }

    /// The first matching block of an own-branch event (changes, checks, issues, mentions).
    fn branch_workflow(&self, event: Event, repo: &str) -> &BranchWorkflowConfig {
        let blocks = match event {
            Event::ChangesRequested => &self.config.changes_requested,
            Event::CiFailing => &self.config.ci_failing,
            Event::Assigned => &self.config.assigned,
            Event::Mentioned => &self.config.mentioned,
            Event::ReviewRequested | Event::ReadyToMerge => return &self.branch_fallback,
        };
        blocks
            .iter()
            .find(|block| block.applies_to(repo))
            .unwrap_or(&self.branch_fallback)
    }

    fn settings(&self, event: Event, repo: &str) -> Settings<'_> {
        if event == Event::ReviewRequested {
            let config = &self.workflow(repo).config;
            return Settings {
                agent: &config.agent,
                agent_args: &config.agent_args,
                editor_command: &config.editor_command,
                lazygit_command: &config.lazygit_command,
                diff_command: &config.diff_command,
                delete_branch: config.delete_branch,
                on_resolved: config.on_resolved,
            };
        }
        let config = self.branch_workflow(event, repo);
        Settings {
            agent: &config.agent,
            agent_args: &config.agent_args,
            editor_command: &config.editor_command,
            lazygit_command: &config.lazygit_command,
            diff_command: &config.viewer_command,
            delete_branch: config.delete_branch,
            on_resolved: config.on_resolved,
        }
    }

    fn query(&self, event: Event) -> &str {
        let queries = &self.config.queries;
        match event {
            Event::ReviewRequested => &queries.review_requested,
            Event::ChangesRequested => &queries.changes_requested,
            Event::CiFailing => &queries.ci_failing,
            Event::Assigned => &queries.assigned,
            Event::Mentioned => &queries.mentioned,
            Event::ReadyToMerge => &queries.ready_to_merge,
        }
    }

    fn repo(&self, name: &str) -> Option<&GithubRepoConfig> {
        self.config
            .repos
            .iter()
            .find(|repo| repo.name.eq_ignore_ascii_case(name))
    }

    fn gh(&self) -> std::process::Command {
        let mut command = crate::noninteractive_process::command(&self.config.gh_path);
        command.envs(gh_env());
        command
    }

    /// Why `mode` cannot be offered for `repo`, if it cannot.
    fn mode_unavailable(
        &self,
        mode: ReviewMode,
        event: Event,
        repo: &str,
        mapped: bool,
    ) -> Option<String> {
        if mode.checks_out() && !mapped {
            return Some(format!("No local checkout configured for {repo}"));
        }
        if mode.needs_agent() && self.settings(event, repo).agent.is_empty() {
            return Some(format!("No agent configured for {repo} {}", event.name()));
        }
        None
    }

    fn run_gh(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let mut command = self.gh();
        command.args(args);
        match run_with_timeout(command, GH_TIMEOUT) {
            Ok(output) if output.status.success() => Ok(output.stdout),
            Ok(output) => Err(gh_error(&output)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err("GitHub CLI not found; install gh or set work_items.github.gh_path".into())
            }
            Err(err) => Err(format!("gh failed: {err}")),
        }
    }

    fn prefetch(&self, repo: &GithubRepoConfig, number: u64) -> Result<(), String> {
        let path = crate::worktree::expand_tilde_absolute_path(&repo.path);
        let mut command = crate::noninteractive_process::command("git");
        command.arg("-C").arg(&path).args([
            "fetch",
            "--no-tags",
            "--quiet",
            &repo.remote,
            &format!("+refs/pull/{number}/head:refs/herdr/pull/{number}"),
        ]);
        match run_with_timeout(command, FETCH_TIMEOUT) {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(failure_detail(&output)),
            Err(err) => Err(err.to_string()),
        }
    }
}

fn gh_env() -> Vec<(String, String)> {
    [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("NO_COLOR", "1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect()
}

fn item_detail<T: serde::de::DeserializeOwned>(item: &WorkItem) -> Option<T> {
    item.detail
        .clone()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Heuristic default: small or documentation-only changes are quicker to review on GitHub;
/// your own work is done locally whenever there is a checkout; mentions open on GitHub.
fn default_choice(
    event: Event,
    detail: Option<&GithubDetail>,
    mapped: bool,
    small_diff_lines: u64,
    docs: &[Regex],
) -> &'static str {
    match event {
        // Ready-to-merge defaults are decided by `merge_choices`.
        Event::Mentioned | Event::ReadyToMerge => return GITHUB_CHOICE_ID,
        _ if !mapped => return GITHUB_CHOICE_ID,
        Event::ChangesRequested => return ReviewMode::Address.choice_id(),
        Event::CiFailing => return ReviewMode::FixChecks.choice_id(),
        Event::Assigned => return ReviewMode::StartIssue.choice_id(),
        Event::ReviewRequested => {}
    }
    let Some(detail) = detail else {
        return ReviewMode::Local.choice_id();
    };
    let docs_only = !detail.files.is_empty()
        && detail
            .files
            .iter()
            .all(|file| docs.iter().any(|pattern| pattern.is_match(&file.path)));
    if docs_only || detail.additions + detail.deletions <= small_diff_lines {
        GITHUB_CHOICE_ID
    } else {
        ReviewMode::Local.choice_id()
    }
}

pub(super) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// How an item is worked on once the user picks a provisioning choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewMode {
    /// Worktree checkout; the agent is briefed and waits.
    Local,
    /// Worktree checkout; the agent reviews straight away and reports back.
    LocalAgentReview,
    /// No checkout; the agent reviews through gh and reports back.
    AgentReport,
    /// No checkout; the agent reviews through gh and posts a review comment.
    AgentPost,
    /// Worktree on the pull request branch; the agent summarises the feedback and waits.
    Address,
    /// Worktree on the pull request branch; the agent addresses the feedback.
    AddressAgent,
    /// Worktree on the pull request branch; the agent diagnoses the failing checks and waits.
    FixChecks,
    /// Worktree on the pull request branch; the agent fixes the failing checks.
    FixChecksAgent,
    /// Worktree on a new issue branch; the agent proposes a plan and waits.
    StartIssue,
    /// Worktree on a new issue branch; the agent implements the issue.
    StartIssueAgent,
    /// No checkout; the agent reads the downloaded thread and drafts a reply.
    ThreadAgent,
}

impl ReviewMode {
    const ALL: [Self; 11] = [
        Self::Local,
        Self::LocalAgentReview,
        Self::AgentReport,
        Self::AgentPost,
        Self::Address,
        Self::AddressAgent,
        Self::FixChecks,
        Self::FixChecksAgent,
        Self::StartIssue,
        Self::StartIssueAgent,
        Self::ThreadAgent,
    ];

    fn choice_id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LocalAgentReview => "local_agent",
            Self::AgentReport => "agent_report",
            Self::AgentPost => "agent_post",
            Self::Address => "address",
            Self::AddressAgent => "address_agent",
            Self::FixChecks => "fix_checks",
            Self::FixChecksAgent => "fix_checks_agent",
            Self::StartIssue => "start_issue",
            Self::StartIssueAgent => "start_issue_agent",
            Self::ThreadAgent => "thread_agent",
        }
    }

    fn from_choice_id(choice_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.choice_id() == choice_id)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Review locally",
            Self::LocalAgentReview => "Review locally, ask agent to review first",
            Self::AgentReport => "Ask agent to review and report back",
            Self::AgentPost => "Ask agent to review and comment on GitHub",
            Self::Address | Self::FixChecks | Self::StartIssue => "Work on it locally",
            Self::AddressAgent => "Ask agent to address the feedback",
            Self::FixChecksAgent => "Ask agent to fix the checks",
            Self::StartIssueAgent => "Ask agent to implement it",
            Self::ThreadAgent => "Ask agent to draft a reply",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Local => "Worktree and tools; the agent gets the context and waits",
            Self::LocalAgentReview => "Worktree and tools; the agent starts reviewing",
            Self::AgentReport => "No checkout; the agent reviews with gh and reports here",
            Self::AgentPost => "No checkout; the agent reviews and comments on the PR",
            Self::Address => "Worktree on the PR branch; the agent sums up the feedback and waits",
            Self::AddressAgent => {
                "Worktree on the PR branch; the agent makes the changes, no commit or push"
            }
            Self::FixChecks => {
                "Worktree on the PR branch; the agent diagnoses the failures and waits"
            }
            Self::FixChecksAgent => {
                "Worktree on the PR branch; the agent fixes the failures, no commit or push"
            }
            Self::StartIssue => "Worktree on a new branch; the agent proposes a plan and waits",
            Self::StartIssueAgent => {
                "Worktree on a new branch; the agent implements it, no commit or push"
            }
            Self::ThreadAgent => "No checkout; nothing is posted to GitHub",
        }
    }

    fn checks_out(self) -> bool {
        !matches!(
            self,
            Self::AgentReport | Self::AgentPost | Self::ThreadAgent
        )
    }

    fn needs_agent(self) -> bool {
        !matches!(
            self,
            Self::Local | Self::Address | Self::FixChecks | Self::StartIssue
        )
    }
}

fn diff_file_name(number: u64) -> String {
    format!("pr-{number}.diff")
}

fn changed_files_list(detail: &GithubDetail) -> String {
    let mut files: Vec<String> = detail
        .files
        .iter()
        .take(MAX_BRIEF_FILES)
        .map(|file| format!("- {} (+{} −{})", file.path, file.additions, file.deletions))
        .collect();
    if detail.files.len() > MAX_BRIEF_FILES {
        files.push(format!(
            "- … and {} more",
            detail.files.len() - MAX_BRIEF_FILES
        ));
    }
    files.join("\n")
}

/// Collapses whitespace and drops HTML tags, which review bots use heavily.
pub(super) fn one_line(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                plain.push(' ');
            }
            _ if !in_tag => plain.push(character),
            _ => {}
        }
    }
    truncate_chars(
        &plain.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_COMMENT_CHARS,
    )
}

/// Reviews that requested changes, then inline comments, as brief lines. Other reviews are
/// left to the full threads the brief points at.
fn feedback_list(detail: &GithubDetail) -> String {
    let mut lines: Vec<String> = detail
        .reviews
        .iter()
        .filter(|review| review.state == "CHANGES_REQUESTED")
        .map(|review| {
            let author = review
                .author
                .as_ref()
                .map_or("unknown", |author| author.login.as_str());
            let state = review.state.to_lowercase().replace('_', " ");
            let body = if review.body.trim().is_empty() {
                "(no summary; see the inline comments)".to_string()
            } else {
                one_line(&review.body)
            };
            format!("- @{author} ({state}): {body}")
        })
        .collect();
    let skipped = detail
        .inline_comments
        .len()
        .saturating_sub(MAX_BRIEF_COMMENTS);
    for comment in detail.inline_comments.iter().skip(skipped) {
        let location = match comment.line {
            Some(line) => format!("{}:{line}", comment.path),
            None => comment.path.clone(),
        };
        lines.push(format!(
            "- {location} @{}: {}",
            comment.author,
            one_line(&comment.body)
        ));
    }
    if skipped > 0 {
        lines.push(format!("- … and {skipped} older inline comments"));
    }
    if lines.is_empty() {
        "(no review text was found)".into()
    } else {
        lines.join("\n")
    }
}

fn changes_brief(repo: &str, item: &WorkItem, detail: &GithubDetail, mode: ReviewMode) -> String {
    let number = detail.number;
    let head = &detail.head_ref_name;
    let instructions = if mode == ReviewMode::AddressAgent {
        "Address each point now: make the changes and run the relevant tests. Do not commit, \
         push or reply on GitHub. Report what you changed and any point you disagree with."
    } else {
        "Summarise what the reviewers asked for, propose how to address each point and wait \
         for my instructions before changing anything."
    };
    format!(
        "Reviewers requested changes on your GitHub pull request {repo}#{number}: {title}\n\
         {url}\n\
         This directory is a worktree on the pull request branch {head} (base {base}).\n\
         \n\
         Review feedback:\n\
         {feedback}\n\
         \n\
         Use `gh pr view {number} --repo {repo} --comments` and \
         `gh api repos/{repo}/pulls/{number}/comments` for the full threads.\n\
         \n\
         Changed files ({changed}, +{additions} −{deletions}):\n\
         {files}\n\
         \n\
         {instructions}",
        title = item.title,
        url = item.url,
        base = detail.base_ref_name,
        feedback = feedback_list(detail),
        changed = detail.changed_files,
        additions = detail.additions,
        deletions = detail.deletions,
        files = changed_files_list(detail),
    )
}

fn ci_brief(repo: &str, item: &WorkItem, detail: &GithubDetail, mode: ReviewMode) -> String {
    let number = detail.number;
    let checks = if detail.failing_checks.is_empty() {
        "(no failing check was listed; see gh pr checks)".to_string()
    } else {
        detail
            .failing_checks
            .iter()
            .map(|check| {
                let name = if check.workflow.is_empty() {
                    check.name.clone()
                } else {
                    format!("{} / {}", check.workflow, check.name)
                };
                format!("- {name}: {}", check.link)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let instructions = if mode == ReviewMode::FixChecksAgent {
        "Fix the failures now and rerun the failing tests locally where you can. Do not commit, \
         push or rerun CI. Report what you changed."
    } else {
        "Find the cause of each failure, explain it and propose a fix, then wait for my \
         instructions before changing anything."
    };
    format!(
        "Checks are failing on your GitHub pull request {repo}#{number}: {title}\n\
         {url}\n\
         This directory is a worktree on the pull request branch {head} (base {base}).\n\
         \n\
         Failing checks:\n\
         {checks}\n\
         \n\
         Read the logs with `gh pr checks {number} --repo {repo}` and \
         `gh run view <run-id> --repo {repo} --log-failed` (the run id is in each link).\n\
         \n\
         {instructions}",
        title = item.title,
        url = item.url,
        head = detail.head_ref_name,
        base = detail.base_ref_name,
    )
}

fn issue_comments_list(detail: &GithubIssueDetail) -> String {
    let skipped = detail.comments.len().saturating_sub(MAX_BRIEF_COMMENTS);
    let mut lines: Vec<String> = detail
        .comments
        .iter()
        .skip(skipped)
        .map(|comment| format!("- @{}: {}", comment.author, one_line(&comment.body)))
        .collect();
    if skipped > 0 {
        lines.insert(0, format!("- … {skipped} older comments"));
    }
    if lines.is_empty() {
        "(no comments)".into()
    } else {
        lines.join("\n")
    }
}

fn issue_brief(
    repo: &str,
    item: &WorkItem,
    detail: &GithubIssueDetail,
    mode: ReviewMode,
    branch: &str,
) -> String {
    let number = detail.number;
    let body = if detail.body.trim().is_empty() {
        "(no description)".to_string()
    } else {
        truncate_chars(detail.body.trim(), MAX_BRIEF_BODY_CHARS)
    };
    let labels = if detail.labels.is_empty() {
        String::new()
    } else {
        format!("Labels: {}\n", detail.labels.join(", "))
    };
    let instructions = if mode == ReviewMode::StartIssueAgent {
        "Implement it now, with tests. Do not commit, push or comment on GitHub. Report what \
         you changed and any open question."
    } else {
        "Investigate the code, propose an implementation plan and wait for my instructions \
         before changing anything."
    };
    format!(
        "You are working on GitHub issue {repo}#{number}: {title}\n\
         {url}\n\
         {labels}\
         This directory is a worktree on the new branch {branch} (from {base}).\n\
         \n\
         Description:\n\
         {body}\n\
         \n\
         Comments:\n\
         {comments}\n\
         \n\
         {instructions}",
        title = item.title,
        url = item.url,
        base = detail.default_branch,
        comments = issue_comments_list(detail),
    )
}

fn thread_file_name(number: u64) -> String {
    format!("thread-{number}.md")
}

fn thread_brief(repo: &str, item: &WorkItem, detail: &GithubIssueDetail) -> String {
    let kind = if detail.is_pull_request {
        "pull request"
    } else {
        "issue"
    };
    format!(
        "You were mentioned in GitHub {kind} {repo}#{number}: {title}\n\
         {url}\n\
         There is no checkout. The whole thread is in ./{file}.\n\
         \n\
         Read it, tell me what is being asked of me and draft a reply. Do not post anything \
         to GitHub.",
        number = detail.number,
        title = item.title,
        url = item.url,
        file = thread_file_name(detail.number),
    )
}

/// Lower-case words of `title` joined by `-`, at most about 40 characters.
pub(super) fn slug(title: &str) -> String {
    let mut slug = String::new();
    for word in title
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if slug.len() + word.len() + 1 > 40 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    slug
}

/// Branch for an issue: `issue/<n>-<words of the title>`.
fn issue_branch(number: u64, title: &str) -> String {
    let slug = slug(title);
    if slug.is_empty() {
        format!("issue/{number}")
    } else {
        format!("issue/{number}-{slug}")
    }
}

fn brief(repo: &str, item: &WorkItem, detail: &GithubDetail, mode: ReviewMode) -> String {
    let author = item.author.as_deref().unwrap_or("unknown");
    let head: String = detail.head_ref_oid.chars().take(8).collect();
    let body = if detail.body.trim().is_empty() {
        "(no description)".to_string()
    } else {
        truncate_chars(detail.body.trim(), MAX_BRIEF_BODY_CHARS)
    };
    let number = detail.number;
    let base = &detail.base_ref_name;
    let gh_context = format!(
        "There is no checkout of the repository. The full diff is in ./{file}; use \
         `gh pr view {number} --repo {repo} --comments`, `gh pr diff {number} --repo {repo}` and \
         `gh api repos/{repo}/contents/<path>?ref={head_ref}` for more context.",
        file = diff_file_name(number),
        head_ref = detail.head_ref_name,
    );
    let review = "Review the change for correctness, security, missing tests and design problems. \
                  Order your findings by severity and cite file:line for each.";
    let instructions = match mode {
        ReviewMode::Local => format!(
            "This directory is a worktree checked out at the pull request head.\n\
             Inspect the change with git (for example `git diff origin/{base}...HEAD`), summarise it and wait for my instructions before changing anything."
        ),
        ReviewMode::LocalAgentReview => format!(
            "This directory is a worktree checked out at the pull request head.\n\
             Start reviewing now: read the diff (`git diff origin/{base}...HEAD`) and the surrounding code. {review}\n\
             Report the findings to me here. Do not modify files, commit, or post anything to GitHub."
        ),
        ReviewMode::AgentReport => format!(
            "{gh_context}\n\
             Start reviewing now. {review}\n\
             Report the findings to me here. Do not post anything to GitHub."
        ),
        ReviewMode::AgentPost => format!(
            "{gh_context}\n\
             Start reviewing now. {review}\n\
             When you are done, show me the findings and post them as one review comment with \
             `gh pr review {number} --repo {repo} --comment --body-file <file>`. \
             Only comment: never approve or request changes."
        ),
        ReviewMode::Address | ReviewMode::AddressAgent => {
            return changes_brief(repo, item, detail, mode)
        }
        ReviewMode::FixChecks | ReviewMode::FixChecksAgent => {
            return ci_brief(repo, item, detail, mode)
        }
        // Issue modes are briefed from issue details by `issue_brief`.
        ReviewMode::StartIssue | ReviewMode::StartIssueAgent | ReviewMode::ThreadAgent => {
            return format!("{} {}", item.title, item.url)
        }
    };
    format!(
        "You are reviewing GitHub pull request {repo}#{number}: {title}\n\
         Author: @{author} · {url}\n\
         Base {base} ← head {head_ref} ({head})\n\
         \n\
         Description:\n\
         {body}\n\
         \n\
         Changed files ({changed}, +{additions} −{deletions}):\n\
         {files}\n\
         \n\
         {instructions}",
        title = item.title,
        url = item.url,
        head_ref = detail.head_ref_name,
        changed = detail.changed_files,
        additions = detail.additions,
        deletions = detail.deletions,
        files = changed_files_list(detail),
    )
}

fn gh_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("gh auth login") {
        return "GitHub CLI is not authenticated; run gh auth login".into();
    }
    format!("gh api failed: {}", failure_detail(output))
}

/// Your open pull requests with each reviewer's latest review, in one call.
const READY_TO_MERGE_GRAPHQL: &str = "query($q: String!) { search(query: $q, type: ISSUE, \
    first: 100) { nodes { ... on PullRequest { number title url updatedAt isDraft \
    author { login } repository { nameWithOwner } reviewDecision \
    latestReviews(first: 50) { nodes { state author { login } } } } } } }";

#[derive(Deserialize)]
struct GraphqlResponse {
    data: GraphqlData,
}

#[derive(Deserialize)]
struct GraphqlData {
    search: GraphqlSearch,
}

#[derive(Deserialize)]
struct GraphqlSearch {
    nodes: Vec<Option<GraphqlPullRequest>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlPullRequest {
    number: u64,
    title: String,
    url: String,
    updated_at: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    author: Option<SearchUser>,
    repository: GraphqlRepository,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    latest_reviews: Option<GraphqlReviews>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlRepository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GraphqlReviews {
    nodes: Vec<Option<GithubReview>>,
}

/// Whether reviews leave nothing to do but merge. GitHub only computes a review decision
/// when the repository requires reviews, so without one the latest reviews decide: at least
/// one approval and nobody still asking for changes.
fn is_approved(decision: Option<&str>, reviews: &[GithubReview]) -> bool {
    match decision.filter(|decision| !decision.is_empty()) {
        Some(decision) => decision == "APPROVED",
        None => {
            reviews.iter().any(|review| review.state == "APPROVED")
                && !reviews
                    .iter()
                    .any(|review| review.state == "CHANGES_REQUESTED")
        }
    }
}

fn parse_ready_to_merge(bytes: &[u8]) -> Result<Vec<SourceItem>, String> {
    let response: GraphqlResponse = serde_json::from_slice(bytes)
        .map_err(|err| format!("unexpected gh api graphql output: {err}"))?;
    Ok(response
        .data
        .search
        .nodes
        .into_iter()
        .flatten()
        .filter(|pull| {
            let reviews: Vec<GithubReview> = pull
                .latest_reviews
                .iter()
                .flat_map(|reviews| reviews.nodes.iter().flatten().cloned())
                .collect();
            !pull.is_draft && is_approved(pull.review_decision.as_deref(), &reviews)
        })
        .map(|pull| {
            let repo = pull.repository.name_with_owner;
            SourceItem {
                external_id: format!("{MERGE_PREFIX}{repo}#{}", pull.number),
                title: pull.title,
                context: format!("#{} {} · {repo}", pull.number, Event::ReadyToMerge.tag()),
                author: pull.author.map(|author| author.login),
                url: pull.url,
                updated_at: pull.updated_at,
            }
        })
        .collect())
}

fn parse_search(bytes: &[u8], event: Event) -> Result<Vec<SourceItem>, String> {
    let response: SearchResponse =
        serde_json::from_slice(bytes).map_err(|err| format!("unexpected gh api output: {err}"))?;
    Ok(response
        .items
        .into_iter()
        .filter_map(|item| {
            let repo = item.repository_url.as_deref().and_then(|url| {
                let mut segments = url.trim_end_matches('/').rsplit('/');
                let name = segments.next()?;
                let owner = segments.next()?;
                (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}"))
            });
            let Some(repo) = repo else {
                warn!(url = %item.html_url, "skipping search result without a repository");
                return None;
            };
            // Number first: the sidebar truncates from the right, and the number is what tells
            // two pull requests of the same repository apart. The event goes before the
            // repository for the same reason.
            let mut context = match event.tag() {
                "" => format!("#{} {repo}", item.number),
                tag => format!("#{} {tag} · {repo}", item.number),
            };
            if item.draft == Some(true) {
                context.push_str(" · draft");
            }
            Some(SourceItem {
                external_id: format!("{}{repo}#{}", event.id_prefix(), item.number),
                title: item.title,
                context,
                author: item.user.map(|user| user.login),
                url: item.html_url,
                updated_at: item.updated_at,
            })
        })
        .collect())
}

impl WorkItemSource for GithubSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn label(&self) -> &str {
        "GitHub"
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(
            self.config
                .poll_interval_seconds
                .clamp(MIN_POLL_SECONDS, MAX_POLL_SECONDS),
        )
    }

    fn poll(&self) -> Result<Vec<SourceItem>, String> {
        if let Some(error) = &self.build_error {
            return Err(error.clone());
        }
        let mut items = Vec::new();
        for event in Event::ALL {
            let query = self.query(event);
            if query.trim().is_empty() {
                continue;
            }
            if event == Event::ReadyToMerge {
                items.extend(self.search_ready_to_merge(query)?);
            } else {
                items.extend(self.search(event, query)?);
            }
        }
        Ok(items)
    }

    fn prepare(&self, item: &SourceItem) -> PreparedItem {
        let Some((repo, number)) = parse_external_id(&item.external_id) else {
            return PreparedItem {
                detail: None,
                summary: None,
                error: Some(format!("unrecognised pull request id {}", item.external_id)),
            };
        };
        let event = Event::of(&item.external_id);
        if !event.is_pull_request_event() {
            return self.prepare_issue(event, repo, number);
        }
        let number_arg = number.to_string();
        let fields = match event {
            Event::ChangesRequested => format!("{DETAIL_FIELDS},isCrossRepository,reviews"),
            Event::CiFailing => format!("{DETAIL_FIELDS},isCrossRepository"),
            Event::ReadyToMerge => format!("{DETAIL_FIELDS},mergeStateStatus,latestReviews"),
            _ => DETAIL_FIELDS.to_string(),
        };
        let viewed = self.run_gh(&["pr", "view", &number_arg, "--repo", repo, "--json", &fields]);
        let mut detail = match viewed.and_then(|stdout| {
            serde_json::from_slice::<GithubDetail>(&stdout)
                .map_err(|err| format!("unexpected gh pr view output: {err}"))
        }) {
            Ok(detail) => detail,
            Err(error) => {
                return PreparedItem {
                    detail: None,
                    summary: None,
                    error: Some(error),
                }
            }
        };
        let mut errors = Vec::new();
        match event {
            Event::ChangesRequested => match self.inline_comments(repo, number) {
                Ok(comments) => detail.inline_comments = comments,
                Err(error) => errors.push(format!("inline comments unavailable: {error}")),
            },
            Event::CiFailing => match self.failing_checks(repo, number) {
                Ok(checks) => detail.failing_checks = checks,
                Err(error) => errors.push(format!("checks unavailable: {error}")),
            },
            Event::ReadyToMerge => match self.merge_methods(repo) {
                Ok(methods) => detail.merge_methods = methods,
                Err(error) => errors.push(format!("merge settings unavailable: {error}")),
            },
            _ => {}
        }
        let size = format!(
            "+{} −{} across {} {}",
            detail.additions,
            detail.deletions,
            detail.changed_files,
            if detail.changed_files == 1 {
                "file"
            } else {
                "files"
            }
        );
        let summary = match event {
            Event::ChangesRequested => {
                let requesting = detail
                    .reviews
                    .iter()
                    .filter(|review| review.state == "CHANGES_REQUESTED")
                    .count();
                format!(
                    "{requesting} {} requesting changes · {} inline comments · {size}",
                    if requesting == 1 { "review" } else { "reviews" },
                    detail.inline_comments.len(),
                )
            }
            Event::ReadyToMerge => merge_summary(&detail),
            Event::CiFailing => {
                let failing = detail.failing_checks.len();
                format!(
                    "{failing} failing {} · {size}",
                    if failing == 1 { "check" } else { "checks" }
                )
            }
            _ => size,
        };
        // Nothing is checked out to merge, so there is no point fetching the change.
        if let Some(error) = self
            .repo(repo)
            .filter(|_| event != Event::ReadyToMerge)
            .and_then(|mapped| self.prefetch(mapped, number).err())
        {
            errors.push(format!("prefetch failed: {error}"));
        }
        PreparedItem {
            detail: serde_json::to_value(&detail).ok(),
            summary: Some(summary),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        let event = Event::of(&item.external_id);
        if event == Event::ReadyToMerge {
            return merge_choices(item, item_detail::<GithubDetail>(item).as_ref());
        }
        let repo = parse_external_id(&item.external_id)
            .map(|(repo, _)| repo)
            .unwrap_or(&item.external_id);
        let mapped = self.repo(repo).is_some();
        let mut choices: Vec<WorkItemChoiceInfo> = event
            .modes()
            .iter()
            .map(|&mode| WorkItemChoiceInfo {
                choice_id: mode.choice_id().into(),
                label: mode.label().into(),
                description: Some(mode.description().into()),
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: self.mode_unavailable(mode, event, repo, mapped),
                confirm: None,
            })
            .collect();
        let (label, url) = match event {
            Event::ReviewRequested => ("Review on GitHub", item.url.clone()),
            Event::CiFailing => ("Open the checks on GitHub", format!("{}/checks", item.url)),
            _ => ("Open on GitHub", item.url.clone()),
        };
        choices.push(WorkItemChoiceInfo {
            choice_id: GITHUB_CHOICE_ID.into(),
            label: label.into(),
            description: Some("Open it in the browser".into()),
            action: WorkItemChoiceAction::OpenUrl { url },
            disabled_reason: None,
            confirm: None,
        });
        let workflow = self.workflow(repo);
        let pull_detail = event
            .is_pull_request_event()
            .then(|| item_detail::<GithubDetail>(item))
            .flatten();
        ItemChoices {
            choices,
            default_choice_id: Some(
                default_choice(
                    event,
                    pull_detail.as_ref(),
                    mapped,
                    workflow.config.small_diff_lines,
                    &workflow.docs,
                )
                .into(),
            ),
        }
    }

    fn provision_plan(
        &self,
        item: &WorkItem,
        choice_id: &str,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        let event = Event::of(&item.external_id);
        let mode = ReviewMode::from_choice_id(choice_id)
            .filter(|mode| event.modes().contains(mode))
            .ok_or_else(|| format!("choice {choice_id} does not provision a workspace"))?;
        let (repo_name, number) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised GitHub id {}", item.external_id))?;
        let mapped = self.repo(repo_name);
        if let Some(reason) = self.mode_unavailable(mode, event, repo_name, mapped.is_some()) {
            return Err(reason);
        }
        let not_ready = || "details are not available yet; try again shortly".to_string();
        let settings = self.settings(event, repo_name);
        let short_name = repo_name.rsplit('/').next().unwrap_or(repo_name);
        let workspace_label = truncate_chars(
            &format!("#{number} {}", item.title),
            MAX_WORKSPACE_LABEL_CHARS,
        );
        let agent_name_hint = match event {
            Event::ReviewRequested => format!("review-{number}"),
            Event::Assigned | Event::Mentioned => format!("issue-{number}"),
            Event::ChangesRequested | Event::CiFailing | Event::ReadyToMerge => {
                format!("pr-{number}")
            }
        };
        let mut layout = WorkspaceLayout {
            agent: settings.agent.to_string(),
            agent_args: settings.agent_args.to_vec(),
            editor_command: settings.editor_command.to_string(),
            lazygit_command: settings.lazygit_command.to_string(),
            diff_command: settings.diff_command.to_string(),
            review_command: String::new(),
        };
        let checkout = mapped.filter(|_| mode.checks_out());
        let (source, brief) = if event.is_pull_request_event() {
            let detail = item_detail::<GithubDetail>(item).ok_or_else(not_ready)?;
            let source = match (checkout, event) {
                (Some(repo), Event::ReviewRequested) => {
                    let (base_refspec, base) = review_base(repo, &detail.base_ref_name);
                    layout.review_command = self
                        .workflow(repo_name)
                        .config
                        .review_command
                        .replace("{base}", &base);
                    WorkspaceSource::Worktree(WorktreeSpec {
                        repo_path: crate::worktree::expand_tilde_absolute_path(&repo.path),
                        remote: repo.remote.clone(),
                        fetch_refspec: pull_refspec(number),
                        base_ref: format!("refs/herdr/pull/{number}"),
                        branch: format!("review/pr-{number}"),
                        reuse_branch: false,
                        extra_fetch_refspecs: base_refspec.into_iter().collect(),
                        adopt_branch_for: None,
                    })
                }
                (Some(repo), _) => WorkspaceSource::Worktree(head_branch_spec(repo, &detail)?),
                (None, _) => WorkspaceSource::Download(DownloadSpec {
                    directory: crate::worktree::default_checkout_path(
                        worktree_directory,
                        short_name,
                        &format!("pr-{number}-agent"),
                    ),
                    program: self.config.gh_path.clone(),
                    args: vec![
                        "pr".into(),
                        "diff".into(),
                        number.to_string(),
                        "--repo".into(),
                        repo_name.into(),
                        "--color".into(),
                        "never".into(),
                    ],
                    env: gh_env(),
                    file_name: diff_file_name(number),
                }),
            };
            (source, brief(repo_name, item, &detail, mode))
        } else {
            let detail = item_detail::<GithubIssueDetail>(item).ok_or_else(not_ready)?;
            match checkout {
                Some(repo) => {
                    let spec = issue_branch_spec(repo, &detail, &item.title)?;
                    let brief = issue_brief(repo_name, item, &detail, mode, &spec.branch);
                    (WorkspaceSource::Worktree(spec), brief)
                }
                None => {
                    let kind = if detail.is_pull_request {
                        "pr"
                    } else {
                        "issue"
                    };
                    let source = WorkspaceSource::Download(DownloadSpec {
                        directory: crate::worktree::default_checkout_path(
                            worktree_directory,
                            short_name,
                            &format!("thread-{number}"),
                        ),
                        program: self.config.gh_path.clone(),
                        args: vec![
                            kind.into(),
                            "view".into(),
                            number.to_string(),
                            "--repo".into(),
                            repo_name.into(),
                            "--comments".into(),
                        ],
                        env: gh_env(),
                        file_name: thread_file_name(number),
                    });
                    (source, thread_brief(repo_name, item, &detail))
                }
            }
        };
        Ok(ProvisionPlan {
            source,
            workspace_label,
            agent_name_hint,
            brief,
            layout,
            delete_branch: settings.delete_branch,
        })
    }

    fn remove_on_resolved(&self, item: &WorkItem) -> bool {
        let repo = parse_external_id(&item.external_id)
            .map(|(repo, _)| repo)
            .unwrap_or(&item.external_id);
        self.settings(Event::of(&item.external_id), repo)
            .on_resolved
            == OnResolvedConfig::Remove
    }

    fn perform(&self, item: &WorkItem, choice_id: &str) -> Result<String, String> {
        if Event::of(&item.external_id) != Event::ReadyToMerge {
            return Err(format!("choice {choice_id} cannot be carried out here"));
        }
        let flag = MERGE_METHODS
            .iter()
            .find(|(_, id, _)| *id == choice_id)
            .map(|(method, _, _)| format!("--{}", method.to_lowercase()))
            .ok_or_else(|| format!("unknown merge choice {choice_id}"))?;
        let (repo, number) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised pull request id {}", item.external_id))?;
        let detail = item_detail::<GithubDetail>(item)
            .ok_or("pull request details are not available yet; try again shortly")?;
        if let Some(blocker) = merge_blocker(&detail) {
            return Err(blocker);
        }
        let number_arg = number.to_string();
        let mut args = vec![
            "pr",
            "merge",
            number_arg.as_str(),
            "--repo",
            repo,
            flag.as_str(),
        ];
        // Only the commit you saw is merged: a push since then makes GitHub refuse.
        if !detail.head_ref_oid.is_empty() {
            args.extend(["--match-head-commit", detail.head_ref_oid.as_str()]);
        }
        self.run_gh(&args)?;
        Ok(format!("Merged #{number} into {}", detail.base_ref_name))
    }

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        let title = Event::of(&item.external_id).arrival_title();
        (
            title.into(),
            Some(format!("{} · {}", item.context, item.title)),
        )
    }
}

const DETAIL_FIELDS: &str = "number,title,body,url,additions,deletions,changedFiles,files,\
                             baseRefName,headRefName,headRefOid,author";

/// Merge methods in GitHub's names, with what the dialog calls them.
const MERGE_METHODS: [(&str, &str, &str); 3] = [
    ("SQUASH", "merge_squash", "Squash and merge"),
    ("MERGE", "merge_commit", "Create a merge commit"),
    ("REBASE", "merge_rebase", "Rebase and merge"),
];

fn approvers(detail: &GithubDetail) -> Vec<&str> {
    detail
        .latest_reviews
        .iter()
        .filter(|review| review.state == "APPROVED")
        .filter_map(|review| review.author.as_ref().map(|author| author.login.as_str()))
        .collect()
}

/// Why GitHub will refuse the merge right now, from its merge state.
fn merge_blocker(detail: &GithubDetail) -> Option<String> {
    let base = &detail.base_ref_name;
    match detail.merge_state_status.as_str() {
        "DIRTY" => Some(format!("Conflicts with {base}; resolve them first")),
        "BEHIND" => Some(format!("Behind {base}; update the branch first")),
        "BLOCKED" => Some("GitHub blocks the merge: required checks or reviews".into()),
        "DRAFT" => Some("Still a draft".into()),
        // CLEAN, HAS_HOOKS, UNSTABLE (optional checks failing) and UNKNOWN (still computing)
        // are left to GitHub to accept or refuse.
        _ => None,
    }
}

fn merge_summary(detail: &GithubDetail) -> String {
    let approvers = approvers(detail);
    let approved = if approvers.is_empty() {
        "Approved".to_string()
    } else {
        format!("Approved by {}", approvers.join(", "))
    };
    let state = match detail.merge_state_status.as_str() {
        "CLEAN" | "HAS_HOOKS" => "ready to merge".to_string(),
        "UNSTABLE" => "ready to merge, optional checks failing".to_string(),
        _ => merge_blocker(detail)
            .map(|blocker| blocker.to_lowercase())
            .unwrap_or_else(|| "ready to merge".into()),
    };
    format!("{approved} · {state}")
}

/// Merge choices, your default method first, then opening it on GitHub.
fn merge_choices(item: &WorkItem, detail: Option<&GithubDetail>) -> ItemChoices {
    let Some(detail) = detail else {
        return ItemChoices {
            choices: vec![open_on_github(item, "Open on GitHub")],
            default_choice_id: Some(GITHUB_CHOICE_ID.into()),
        };
    };
    let blocker = merge_blocker(detail);
    let base = &detail.base_ref_name;
    let number = detail.number;
    let mut choices: Vec<WorkItemChoiceInfo> = detail
        .merge_methods
        .iter()
        .filter_map(|method| MERGE_METHODS.iter().find(|(name, _, _)| name == method))
        .map(|(_, choice_id, label)| WorkItemChoiceInfo {
            choice_id: (*choice_id).into(),
            label: (*label).into(),
            description: Some(format!("Merge #{number} into {base} on GitHub")),
            action: WorkItemChoiceAction::Perform,
            disabled_reason: blocker.clone(),
            confirm: Some(format!(
                "Merge #{number} into {base}? This cannot be undone. Press ↵ again to merge."
            )),
        })
        .collect();
    let default = choices
        .iter()
        .find(|choice| choice.disabled_reason.is_none())
        .map_or(GITHUB_CHOICE_ID.to_string(), |choice| {
            choice.choice_id.clone()
        });
    choices.push(open_on_github(item, "Open on GitHub"));
    ItemChoices {
        choices,
        default_choice_id: Some(default),
    }
}

fn open_on_github(item: &WorkItem, label: &str) -> WorkItemChoiceInfo {
    WorkItemChoiceInfo {
        choice_id: GITHUB_CHOICE_ID.into(),
        label: label.into(),
        description: Some("Open it in the browser".into()),
        action: WorkItemChoiceAction::OpenUrl {
            url: item.url.clone(),
        },
        disabled_reason: None,
        confirm: None,
    }
}

/// The pull request's base as a ref reviewers can diff against, and the refspec that
/// fetches it. A named remote updates its remote-tracking branch, so the ref reads the way
/// people type it (`origin/main`); a URL remote gets a private ref.
fn review_base(repo: &GithubRepoConfig, base: &str) -> (Option<String>, String) {
    if base.is_empty() {
        return (None, "HEAD".into());
    }
    if is_remote_name(&repo.remote) {
        (
            Some(format!(
                "+refs/heads/{base}:refs/remotes/{remote}/{base}",
                remote = repo.remote
            )),
            format!("{}/{base}", repo.remote),
        )
    } else {
        (
            Some(format!("+refs/heads/{base}:refs/herdr/base/{base}")),
            format!("refs/herdr/base/{base}"),
        )
    }
}

fn pull_refspec(number: u64) -> String {
    format!("+refs/pull/{number}/head:refs/herdr/pull/{number}")
}

/// A remote given by name (not URL) has remote-tracking branches a new branch can track.
fn is_remote_name(remote: &str) -> bool {
    !remote.is_empty() && !remote.contains(['/', ':', '\\'])
}

/// Worktree on the pull request's own head branch, reusing a local branch of that name.
/// Same-repository heads are fetched into the remote-tracking branch so a new local
/// branch tracks it and `git push` works; fork heads come from `refs/pull/N/head`.
fn head_branch_spec(
    repo: &GithubRepoConfig,
    detail: &GithubDetail,
) -> Result<WorktreeSpec, String> {
    let head = &detail.head_ref_name;
    if head.is_empty() {
        return Err("the pull request has no head branch name".into());
    }
    let number = detail.number;
    let (fetch_refspec, base_ref) = if !detail.is_cross_repository && is_remote_name(&repo.remote) {
        (
            format!(
                "+refs/heads/{head}:refs/remotes/{remote}/{head}",
                remote = repo.remote
            ),
            format!("{}/{head}", repo.remote),
        )
    } else {
        (pull_refspec(number), format!("refs/herdr/pull/{number}"))
    };
    Ok(WorktreeSpec {
        repo_path: crate::worktree::expand_tilde_absolute_path(&repo.path),
        remote: repo.remote.clone(),
        fetch_refspec,
        base_ref,
        branch: head.clone(),
        reuse_branch: true,
        extra_fetch_refspecs: Vec::new(),
        adopt_branch_for: None,
    })
}

/// Worktree on a new branch for an issue, starting at the repository's default branch.
/// The base is a private ref rather than the remote-tracking branch, so the new branch
/// does not track the default branch and `git push` cannot land on it by accident.
fn issue_branch_spec(
    repo: &GithubRepoConfig,
    detail: &GithubIssueDetail,
    title: &str,
) -> Result<WorktreeSpec, String> {
    let default = &detail.default_branch;
    if default.is_empty() {
        return Err("the repository's default branch is not known yet; try again shortly".into());
    }
    let fetch_refspec = format!("+refs/heads/{default}:refs/herdr/base/{default}");
    let base_ref = format!("refs/herdr/base/{default}");
    Ok(WorktreeSpec {
        repo_path: crate::worktree::expand_tilde_absolute_path(&repo.path),
        remote: repo.remote.clone(),
        fetch_refspec,
        base_ref,
        branch: issue_branch(detail.number, title),
        reuse_branch: true,
        extra_fetch_refspecs: Vec::new(),
        adopt_branch_for: None,
    })
}

impl GithubSource {
    /// Results of one event's search, newest first, up to `max_results` (GitHub serves at
    /// most 1000 per query).
    fn search(&self, event: Event, query: &str) -> Result<Vec<SourceItem>, String> {
        let limit = self.config.max_results.clamp(1, MAX_SEARCH_RESULTS);
        let per_page = limit.min(SEARCH_PAGE_SIZE);
        let query = format!("q={query}");
        let per_page_arg = format!("per_page={per_page}");
        let mut items = Vec::new();
        for page in 1.. {
            let page_arg = format!("page={page}");
            let stdout = self.run_gh(&[
                "api",
                "--method",
                "GET",
                "search/issues",
                "-f",
                &query,
                "-f",
                &per_page_arg,
                "-f",
                &page_arg,
                "-f",
                "sort=updated",
                "-f",
                "order=desc",
            ])?;
            let found = parse_search(&stdout, event)?;
            let full_page = found.len() == per_page;
            items.extend(found);
            if !full_page || items.len() >= limit {
                break;
            }
        }
        items.truncate(limit);
        Ok(items)
    }

    /// Merge methods the repository allows, your default first.
    fn merge_methods(&self, repo: &str) -> Result<Vec<String>, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Settings {
            #[serde(default)]
            squash_merge_allowed: bool,
            #[serde(default)]
            merge_commit_allowed: bool,
            #[serde(default)]
            rebase_merge_allowed: bool,
            #[serde(default)]
            viewer_default_merge_method: String,
        }
        let stdout = self.run_gh(&[
            "repo",
            "view",
            repo,
            "--json",
            "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed,viewerDefaultMergeMethod",
        ])?;
        let settings: Settings = serde_json::from_slice(&stdout)
            .map_err(|err| format!("unexpected gh repo view output: {err}"))?;
        let mut methods: Vec<String> = [
            ("SQUASH", settings.squash_merge_allowed),
            ("MERGE", settings.merge_commit_allowed),
            ("REBASE", settings.rebase_merge_allowed),
        ]
        .into_iter()
        .filter(|(_, allowed)| *allowed)
        .map(|(method, _)| method.to_string())
        .collect();
        if let Some(index) = methods
            .iter()
            .position(|method| *method == settings.viewer_default_merge_method)
        {
            let default = methods.remove(index);
            methods.insert(0, default);
        }
        Ok(methods)
    }

    /// Your open pull requests that reviews approved. Uses GraphQL so the latest reviews come
    /// with the search: GitHub's `review:approved` misses repositories without required reviews.
    fn search_ready_to_merge(&self, query: &str) -> Result<Vec<SourceItem>, String> {
        let query_arg = format!("query={READY_TO_MERGE_GRAPHQL}");
        let search_arg = format!("q={query}");
        let stdout = self.run_gh(&["api", "graphql", "-f", &query_arg, "-f", &search_arg])?;
        parse_ready_to_merge(&stdout)
    }

    /// Issue or mention details through the REST API, which serves pull requests too.
    fn prepare_issue(&self, event: Event, repo: &str, number: u64) -> PreparedItem {
        let issue = self
            .run_gh(&["api", &format!("repos/{repo}/issues/{number}")])
            .and_then(|stdout| {
                serde_json::from_slice::<RestIssue>(&stdout)
                    .map_err(|err| format!("unexpected gh api output: {err}"))
            });
        let issue = match issue {
            Ok(issue) => issue,
            Err(error) => {
                return PreparedItem {
                    detail: None,
                    summary: None,
                    error: Some(error),
                }
            }
        };
        let mut errors = Vec::new();
        let comments = self
            .run_gh(&[
                "api",
                "--method",
                "GET",
                &format!("repos/{repo}/issues/{number}/comments"),
                "-f",
                "per_page=100",
            ])
            .and_then(|stdout| {
                serde_json::from_slice::<Vec<RestIssueComment>>(&stdout)
                    .map_err(|err| format!("unexpected gh api output: {err}"))
            })
            .unwrap_or_else(|error| {
                errors.push(format!("comments unavailable: {error}"));
                Vec::new()
            });
        let default_branch = if event == Event::Assigned {
            self.run_gh(&["api", &format!("repos/{repo}")])
                .and_then(|stdout| {
                    serde_json::from_slice::<RestRepository>(&stdout)
                        .map_err(|err| format!("unexpected gh api output: {err}"))
                })
                .map(|repository| repository.default_branch)
                .unwrap_or_else(|error| {
                    errors.push(format!("default branch unavailable: {error}"));
                    String::new()
                })
        } else {
            String::new()
        };
        let detail = GithubIssueDetail {
            number: issue.number,
            body: issue.body.unwrap_or_default(),
            is_pull_request: issue.pull_request.is_some(),
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            comments: comments
                .into_iter()
                .map(|comment| GithubIssueComment {
                    author: comment
                        .user
                        .map_or_else(|| "unknown".into(), |user| user.login),
                    body: comment.body,
                })
                .collect(),
            default_branch,
        };
        let mut summary = format!(
            "{} {}",
            detail.comments.len(),
            if detail.comments.len() == 1 {
                "comment"
            } else {
                "comments"
            }
        );
        if !detail.labels.is_empty() {
            summary.push_str(&format!(" · {}", detail.labels.join(", ")));
        }
        PreparedItem {
            detail: serde_json::to_value(&detail).ok(),
            summary: Some(summary),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    /// Checks of a pull request that failed or were cancelled. `gh pr checks` exits
    /// non-zero exactly when some fail, so its exit status is not an error here.
    fn failing_checks(&self, repo: &str, number: u64) -> Result<Vec<GithubCheck>, String> {
        let mut command = self.gh();
        command.args([
            "pr",
            "checks",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            "name,state,bucket,link,workflow",
        ]);
        let output =
            run_with_timeout(command, GH_TIMEOUT).map_err(|err| format!("gh failed: {err}"))?;
        let checks: Vec<GithubCheck> = serde_json::from_slice(&output.stdout).map_err(|_| {
            if output.status.success() {
                "unexpected gh pr checks output".to_string()
            } else {
                gh_error(&output)
            }
        })?;
        Ok(checks
            .into_iter()
            .filter(|check| matches!(check.bucket.as_str(), "fail" | "cancel"))
            .collect())
    }

    /// Inline review comments, oldest first.
    fn inline_comments(&self, repo: &str, number: u64) -> Result<Vec<GithubInlineComment>, String> {
        let stdout = self.run_gh(&[
            "api",
            "--method",
            "GET",
            &format!("repos/{repo}/pulls/{number}/comments"),
            "-f",
            "per_page=100",
        ])?;
        let comments: Vec<RestReviewComment> = serde_json::from_slice(&stdout)
            .map_err(|err| format!("unexpected gh api output: {err}"))?;
        Ok(comments
            .into_iter()
            .map(|comment| GithubInlineComment {
                path: comment.path,
                line: comment.line.or(comment.original_line),
                author: comment
                    .user
                    .map_or_else(|| "unknown".into(), |user| user.login),
                body: comment.body,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(unix)]
    fn output(stderr: &str, code: i32) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn search_results_become_source_items() {
        let json = br#"{"total_count":2,"items":[
            {"number":12,"title":"Fix it","html_url":"https://github.com/o/r/pull/12",
             "updated_at":"2026-01-02T00:00:00Z","draft":false,"user":{"login":"alice"},
             "repository_url":"https://api.github.com/repos/o/r"},
            {"number":3,"title":"WIP","html_url":"https://github.com/x/y/pull/3",
             "updated_at":"2026-01-01T00:00:00Z","draft":true,"user":{"login":"bob"},
             "repository_url":"https://api.github.com/repos/x/y"}]}"#;
        let items = parse_search(json, Event::ReviewRequested).expect("parses");
        assert_eq!(
            items,
            vec![
                SourceItem {
                    external_id: "o/r#12".into(),
                    title: "Fix it".into(),
                    context: "#12 o/r".into(),
                    author: Some("alice".into()),
                    url: "https://github.com/o/r/pull/12".into(),
                    updated_at: "2026-01-02T00:00:00Z".into(),
                },
                SourceItem {
                    external_id: "x/y#3".into(),
                    title: "WIP".into(),
                    context: "#3 x/y · draft".into(),
                    author: Some("bob".into()),
                    url: "https://github.com/x/y/pull/3".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                },
            ]
        );
    }

    #[test]
    fn search_result_without_repository_is_skipped() {
        let json = br#"{"items":[{"number":1,"title":"t","html_url":"u","updated_at":"x"}]}"#;
        assert_eq!(
            parse_search(json, Event::ReviewRequested).expect("parses"),
            Vec::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unauthenticated_gh_is_reported_with_the_login_hint() {
        let message = gh_error(&output(
            "To get started with GitHub CLI, please run:  gh auth login\n",
            4,
        ));
        assert_eq!(
            message,
            "GitHub CLI is not authenticated; run gh auth login"
        );
    }

    #[cfg(unix)]
    #[test]
    fn other_gh_failures_report_the_last_stderr_line() {
        let message = gh_error(&output("warning\nHTTP 422: Validation Failed\n", 1));
        assert_eq!(message, "gh api failed: HTTP 422: Validation Failed");
    }

    fn detail(files: &[(&str, u64, u64)]) -> GithubDetail {
        GithubDetail {
            number: 5,
            body: "Adds the thing.".into(),
            additions: files.iter().map(|file| file.1).sum(),
            deletions: files.iter().map(|file| file.2).sum(),
            changed_files: files.len() as u64,
            files: files
                .iter()
                .map(|(path, additions, deletions)| GithubFile {
                    path: (*path).into(),
                    additions: *additions,
                    deletions: *deletions,
                })
                .collect(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            head_ref_oid: "0123456789abcdef".into(),
            is_cross_repository: false,
            reviews: Vec::new(),
            inline_comments: Vec::new(),
            failing_checks: Vec::new(),
            merge_state_status: String::new(),
            latest_reviews: Vec::new(),
            merge_methods: Vec::new(),
        }
    }

    fn work_item(repo: &str, detail: Option<&GithubDetail>) -> WorkItem {
        WorkItem {
            key: format!("github:{repo}#5"),
            source_id: "github".into(),
            external_id: format!("{repo}#5"),
            title: "Add the thing".into(),
            context: format!("#5 {repo}"),
            author: Some("alice".into()),
            url: format!("https://github.com/{repo}/pull/5"),
            updated_at: "2026-01-01T00:00:00Z".into(),
            detail: detail.map(|detail| serde_json::to_value(detail).unwrap()),
            summary: None,
            prepare_error: None,
            phase: crate::api::schema::WorkItemPhase::Pending,
            seen: false,
            resolved: false,
            workspace_id: None,
            dismissed: false,
            snoozed_until: None,
            prepared_for: None,
            prepare_in_flight: false,
            provisioning: None,
            resolve_error: None,
            action_in_flight: false,
            action_error: None,
        }
    }

    fn mapped_source(repo: GithubRepoConfig) -> GithubSource {
        GithubSource::new(GithubWorkItemsConfig {
            repos: vec![repo],
            ..GithubWorkItemsConfig::default()
        })
    }

    fn repo_config() -> GithubRepoConfig {
        GithubRepoConfig {
            name: "o/r".into(),
            path: "/src/r".into(),
            remote: "origin".into(),
        }
    }

    fn default_for(source: &GithubSource, detail: &GithubDetail) -> Option<String> {
        source
            .choices(&work_item("o/r", Some(detail)))
            .default_choice_id
    }

    #[test]
    fn unmapped_repository_defaults_to_github_and_disables_local_review() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let choices = source.choices(&work_item("o/r", Some(&detail(&[("src/a.rs", 500, 0)]))));
        assert_eq!(choices.default_choice_id.as_deref(), Some("github"));
        let local = &choices.choices[0];
        assert_eq!(local.choice_id, "local");
        assert_eq!(
            local.disabled_reason.as_deref(),
            Some("No local checkout configured for o/r")
        );
    }

    #[test]
    fn documentation_only_change_defaults_to_github() {
        let source = mapped_source(repo_config());
        let docs = detail(&[("README.md", 300, 10), ("docs/guide/setup.txt", 200, 0)]);
        assert_eq!(default_for(&source, &docs).as_deref(), Some("github"));
    }

    #[test]
    fn small_change_defaults_to_github() {
        let source = mapped_source(repo_config());
        assert_eq!(
            default_for(&source, &detail(&[("src/a.rs", 15, 5)])).as_deref(),
            Some("github")
        );
    }

    #[test]
    fn large_code_change_defaults_to_local_review() {
        let source = mapped_source(repo_config());
        assert_eq!(
            default_for(&source, &detail(&[("src/a.rs", 15, 6)])).as_deref(),
            Some("local")
        );
    }

    #[test]
    fn local_review_plans_a_review_branch_from_the_fetched_pull_request() {
        let source = mapped_source(repo_config());
        let change = detail(&[("src/a.rs", 40, 2), ("src/b.rs", 1, 1)]);
        let plan = source
            .provision_plan(
                &work_item("o/r", Some(&change)),
                "local",
                Path::new("/worktrees"),
            )
            .expect("plan");
        assert_eq!(
            plan.source,
            WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: PathBuf::from("/src/r"),
                remote: "origin".into(),
                fetch_refspec: "+refs/pull/5/head:refs/herdr/pull/5".into(),
                base_ref: "refs/herdr/pull/5".into(),
                branch: "review/pr-5".into(),
                reuse_branch: false,
                extra_fetch_refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".into()],
                adopt_branch_for: None,
            })
        );
        assert_eq!(
            plan.layout.review_command,
            "{plugin:persiyanov.reviewr}/bin/herdr-reviewr --base origin/main"
        );
        assert!(plan.delete_branch);
        assert!(plan.brief.contains("Add the thing"));
        assert!(plan.brief.contains("- src/a.rs (+40 −2)"));
        assert!(plan.brief.contains("- src/b.rs (+1 −1)"));
    }

    #[test]
    fn matching_review_requested_block_sets_layout_and_branch_policy() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            repos: vec![repo_config()],
            review_requested: vec![
                ReviewRequestedConfig {
                    repos: vec!["other/repo".into()],
                    agent: "codex".into(),
                    ..ReviewRequestedConfig::default()
                },
                ReviewRequestedConfig {
                    repos: vec!["o/r".into()],
                    delete_branch: false,
                    on_resolved: OnResolvedConfig::Remove,
                    editor_command: "hx .".into(),
                    agent_args: vec!["--model".into(), "opus".into()],
                    ..ReviewRequestedConfig::default()
                },
            ],
            ..GithubWorkItemsConfig::default()
        });
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let plan = source
            .provision_plan(&item, "local", Path::new("/worktrees"))
            .expect("plan");
        assert!(!plan.delete_branch);
        assert_eq!(plan.layout.editor_command, "hx .");
        assert_eq!(plan.layout.agent, "claude");
        assert_eq!(plan.layout.agent_args, ["--model", "opus"]);
        assert!(source.remove_on_resolved(&item));
        assert!(!source.remove_on_resolved(&work_item("x/y", None)));
    }

    #[test]
    fn agent_review_needs_no_checkout_and_downloads_the_diff_with_gh() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let change = detail(&[("src/a.rs", 40, 2)]);
        let item = work_item("o/r", Some(&change));
        let choices = source.choices(&item);
        let report = choices
            .choices
            .iter()
            .find(|choice| choice.choice_id == "agent_report")
            .expect("agent review offered");
        assert_eq!(report.disabled_reason, None);
        let plan = source
            .provision_plan(&item, "agent_report", Path::new("/worktrees"))
            .expect("plan");
        let WorkspaceSource::Download(download) = &plan.source else {
            panic!("agent review downloads the diff");
        };
        assert_eq!(download.directory, Path::new("/worktrees/r/pr-5-agent"));
        assert_eq!(download.program, "gh");
        assert_eq!(download.args[..5], ["pr", "diff", "5", "--repo", "o/r"]);
        assert_eq!(download.file_name, "pr-5.diff");
        assert!(plan.brief.contains("./pr-5.diff"));
        assert!(plan.brief.contains("Do not post anything to GitHub"));
    }

    #[test]
    fn posting_agent_review_asks_for_a_comment_only_review() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let plan = source
            .provision_plan(&item, "agent_post", Path::new("/worktrees"))
            .expect("plan");
        assert!(plan
            .brief
            .contains("gh pr review 5 --repo o/r --comment --body-file"));
        assert!(plan.brief.contains("never approve or request changes"));
    }

    #[test]
    fn local_agent_review_briefs_the_agent_to_start_reviewing() {
        let source = mapped_source(repo_config());
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let plan = source
            .provision_plan(&item, "local_agent", Path::new("/worktrees"))
            .expect("plan");
        assert!(matches!(plan.source, WorkspaceSource::Worktree(_)));
        assert!(plan.brief.contains("Start reviewing now"));
        assert!(!plan.brief.contains("wait for my instructions"));
    }

    #[test]
    fn agent_led_choices_are_disabled_without_an_agent() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            repos: vec![repo_config()],
            review_requested: vec![ReviewRequestedConfig {
                agent: String::new(),
                ..ReviewRequestedConfig::default()
            }],
            ..GithubWorkItemsConfig::default()
        });
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let choices = source.choices(&item);
        let disabled: Vec<&str> = choices
            .choices
            .iter()
            .filter(|choice| choice.disabled_reason.is_some())
            .map(|choice| choice.choice_id.as_str())
            .collect();
        assert_eq!(disabled, vec!["local_agent", "agent_report", "agent_post"]);
        assert!(source
            .provision_plan(&item, "agent_report", Path::new("/worktrees"))
            .is_err());
    }

    #[test]
    fn invalid_pattern_is_surfaced_by_poll() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            gh_path: "/nonexistent/gh".into(),
            review_requested: vec![ReviewRequestedConfig {
                docs_patterns: vec!["(".into()],
                ..ReviewRequestedConfig::default()
            }],
            ..GithubWorkItemsConfig::default()
        });
        let error = source.poll().expect_err("poll fails");
        assert!(
            error.starts_with("invalid docs_patterns pattern"),
            "{error}"
        );
    }

    fn changes_item(detail: Option<&GithubDetail>) -> WorkItem {
        WorkItem {
            key: "github:changes:o/r#5".into(),
            external_id: "changes:o/r#5".into(),
            context: "#5 changes · o/r".into(),
            ..work_item("o/r", detail)
        }
    }

    fn feedback_detail() -> GithubDetail {
        GithubDetail {
            reviews: vec![
                GithubReview {
                    author: Some(GithubLogin {
                        login: "bob".into(),
                    }),
                    state: "CHANGES_REQUESTED".into(),
                    body: "Please add a test\nfor the empty case.".into(),
                },
                GithubReview {
                    author: Some(GithubLogin {
                        login: "carol".into(),
                    }),
                    state: "COMMENTED".into(),
                    body: "<details><summary>Nitpicks</summary></details>".into(),
                },
            ],
            inline_comments: vec![GithubInlineComment {
                path: "src/a.rs".into(),
                line: Some(12),
                author: "bob".into(),
                body: "This can panic.".into(),
            }],
            ..detail(&[("src/a.rs", 40, 2)])
        }
    }

    #[test]
    fn changes_requested_results_get_their_own_ids() {
        let json = br#"{"items":[{"number":12,"title":"Fix it","html_url":"https://github.com/o/r/pull/12",
            "updated_at":"2026-01-02T00:00:00Z","user":{"login":"me"},
            "repository_url":"https://api.github.com/repos/o/r"}]}"#;
        let items = parse_search(json, Event::ChangesRequested).expect("parses");
        assert_eq!(items[0].external_id, "changes:o/r#12");
        assert_eq!(items[0].context, "#12 changes · o/r");
        assert_eq!(parse_external_id(&items[0].external_id), Some(("o/r", 12)));
    }

    #[test]
    fn changes_requested_offers_working_on_the_branch_by_default() {
        let source = mapped_source(repo_config());
        let choices = source.choices(&changes_item(Some(&feedback_detail())));
        let ids: Vec<&str> = choices
            .choices
            .iter()
            .map(|choice| choice.choice_id.as_str())
            .collect();
        assert_eq!(ids, vec!["address", "address_agent", "github"]);
        assert_eq!(choices.default_choice_id.as_deref(), Some("address"));
        assert!(source
            .provision_plan(
                &changes_item(Some(&feedback_detail())),
                "local",
                Path::new("/worktrees")
            )
            .is_err());
    }

    #[test]
    fn changes_requested_worktree_tracks_the_pull_request_branch() {
        let source = mapped_source(repo_config());
        let plan = source
            .provision_plan(
                &changes_item(Some(&feedback_detail())),
                "address_agent",
                Path::new("/worktrees"),
            )
            .expect("plan");
        assert_eq!(
            plan.source,
            WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: PathBuf::from("/src/r"),
                remote: "origin".into(),
                fetch_refspec: "+refs/heads/feature:refs/remotes/origin/feature".into(),
                base_ref: "origin/feature".into(),
                branch: "feature".into(),
                reuse_branch: true,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: None,
            })
        );
        assert!(!plan.delete_branch);
        assert!(plan
            .brief
            .contains("- @bob (changes requested): Please add a test for the empty case."));
        assert!(plan.brief.contains("- src/a.rs:12 @bob: This can panic."));
        assert!(!plan.brief.contains("@carol"));
        assert!(plan
            .brief
            .contains("Do not commit, push or reply on GitHub"));
    }

    #[test]
    fn fork_pull_request_branch_starts_from_the_pull_ref() {
        let source = mapped_source(repo_config());
        let fork = GithubDetail {
            is_cross_repository: true,
            ..feedback_detail()
        };
        let plan = source
            .provision_plan(&changes_item(Some(&fork)), "address", Path::new("/w"))
            .expect("plan");
        let WorkspaceSource::Worktree(spec) = plan.source else {
            panic!("worktree");
        };
        assert_eq!(spec.fetch_refspec, "+refs/pull/5/head:refs/herdr/pull/5");
        assert_eq!(spec.base_ref, "refs/herdr/pull/5");
        assert_eq!(spec.branch, "feature");
        assert!(plan.brief.contains("wait for my instructions"));
    }

    #[test]
    fn changes_requested_workflow_is_configured_separately() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            repos: vec![repo_config()],
            changes_requested: vec![BranchWorkflowConfig {
                agent: String::new(),
                on_resolved: OnResolvedConfig::Remove,
                ..BranchWorkflowConfig::default()
            }],
            ..GithubWorkItemsConfig::default()
        });
        let item = changes_item(Some(&feedback_detail()));
        let agent_choice = source
            .choices(&item)
            .choices
            .into_iter()
            .find(|choice| choice.choice_id == "address_agent")
            .expect("offered");
        assert_eq!(
            agent_choice.disabled_reason.as_deref(),
            Some("No agent configured for o/r changes requested")
        );
        assert!(source.remove_on_resolved(&item));
        assert!(!source.remove_on_resolved(&work_item("o/r", None)));
        assert_eq!(
            source
                .arrival_notice(&SourceItem {
                    external_id: item.external_id.clone(),
                    title: item.title.clone(),
                    context: item.context.clone(),
                    author: None,
                    url: item.url.clone(),
                    updated_at: item.updated_at.clone(),
                })
                .0,
            "Changes requested"
        );
    }

    fn event_item(prefix: &str, detail: serde_json::Value) -> WorkItem {
        WorkItem {
            key: format!("github:{prefix}o/r#5"),
            external_id: format!("{prefix}o/r#5"),
            title: "Crash when saving: empty name".into(),
            detail: Some(detail),
            ..work_item("o/r", None)
        }
    }

    fn issue_detail(default_branch: &str) -> GithubIssueDetail {
        GithubIssueDetail {
            number: 5,
            body: "Saving with an empty name panics.".into(),
            is_pull_request: false,
            labels: vec!["bug".into()],
            comments: vec![GithubIssueComment {
                author: "carol".into(),
                body: "Seen on <b>main</b> too.".into(),
            }],
            default_branch: default_branch.into(),
        }
    }

    #[test]
    fn every_event_round_trips_through_its_id_prefix() {
        for event in Event::ALL {
            let id = format!("{}o/r#5", event.id_prefix());
            assert_eq!(Event::of(&id), event);
            assert_eq!(parse_external_id(&id), Some(("o/r", 5)));
        }
    }

    #[test]
    fn failing_checks_work_on_the_branch_and_brief_the_failures() {
        let source = mapped_source(repo_config());
        let detail = GithubDetail {
            failing_checks: vec![GithubCheck {
                name: "rspec".into(),
                state: "FAILURE".into(),
                bucket: "fail".into(),
                link: "https://github.com/o/r/actions/runs/42/job/7".into(),
                workflow: "CI".into(),
            }],
            ..detail(&[("src/a.rs", 3, 1)])
        };
        let item = event_item(CI_PREFIX, serde_json::to_value(&detail).unwrap());
        let choices = source.choices(&item);
        assert_eq!(choices.default_choice_id.as_deref(), Some("fix_checks"));
        let github = choices.choices.last().expect("github choice");
        assert_eq!(
            github.action,
            WorkItemChoiceAction::OpenUrl {
                url: "https://github.com/o/r/pull/5/checks".into()
            }
        );
        let plan = source
            .provision_plan(&item, "fix_checks_agent", Path::new("/w"))
            .expect("plan");
        let WorkspaceSource::Worktree(spec) = &plan.source else {
            panic!("worktree");
        };
        assert_eq!(spec.branch, "feature");
        assert!(spec.reuse_branch);
        assert!(plan
            .brief
            .contains("- CI / rspec: https://github.com/o/r/actions/runs/42/job/7"));
        assert!(plan.brief.contains("Do not commit"));
    }

    #[test]
    fn assigned_issue_starts_a_new_branch_from_the_default_branch() {
        let source = mapped_source(repo_config());
        let item = event_item(
            ASSIGNED_PREFIX,
            serde_json::to_value(issue_detail("main")).unwrap(),
        );
        assert_eq!(
            source.choices(&item).default_choice_id.as_deref(),
            Some("start_issue")
        );
        let plan = source
            .provision_plan(&item, "start_issue", Path::new("/w"))
            .expect("plan");
        assert_eq!(
            plan.source,
            WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: PathBuf::from("/src/r"),
                remote: "origin".into(),
                fetch_refspec: "+refs/heads/main:refs/herdr/base/main".into(),
                base_ref: "refs/herdr/base/main".into(),
                branch: "issue/5-crash-when-saving-empty-name".into(),
                reuse_branch: true,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: None,
            })
        );
        assert!(plan.brief.contains("Saving with an empty name panics."));
        assert!(plan.brief.contains("- @carol: Seen on main too."));
        assert!(plan.brief.contains("wait for my instructions"));
        let unknown_base = event_item(
            ASSIGNED_PREFIX,
            serde_json::to_value(issue_detail("")).unwrap(),
        );
        assert!(source
            .provision_plan(&unknown_base, "start_issue", Path::new("/w"))
            .is_err());
    }

    #[test]
    fn mention_downloads_the_thread_without_a_checkout() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let item = event_item(
            MENTION_PREFIX,
            serde_json::to_value(GithubIssueDetail {
                is_pull_request: true,
                ..issue_detail("")
            })
            .unwrap(),
        );
        let choices = source.choices(&item);
        assert_eq!(choices.default_choice_id.as_deref(), Some("github"));
        let plan = source
            .provision_plan(&item, "thread_agent", Path::new("/w"))
            .expect("plan");
        let WorkspaceSource::Download(download) = &plan.source else {
            panic!("download");
        };
        assert_eq!(download.args[..2], ["pr", "view"]);
        assert_eq!(download.file_name, "thread-5.md");
        assert_eq!(plan.layout.diff_command, "nvim -R {file}");
        assert!(plan
            .brief
            .contains("mentioned in GitHub pull request o/r#5"));
        assert!(plan.brief.contains("Do not post anything"));
    }

    /// A `gh` stand-in answering `search/issues` with `total` results in pages.
    #[cfg(unix)]
    fn fake_gh(name: &str, total: usize) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-fake-gh-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
page=1; per=30
for arg in "$@"; do
  case "$arg" in page=*) page=${{arg#page=}};; per_page=*) per=${{arg#per_page=}};; esac
done
echo "$page" >> "{log}"
start=$(( (page - 1) * per + 1 )); end=$(( page * per )); [ $end -gt {total} ] && end={total}
printf '{{"items":['
n=$start; sep=
while [ $n -le $end ]; do
  printf '%s{{"number":%d,"title":"t","html_url":"u","updated_at":"x","repository_url":"https://api.github.com/repos/o/r"}}' "$sep" $n
  sep=,; n=$((n + 1))
done
printf ']}}'
"#,
                log = dir.join("pages").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn search_pages_until_the_results_run_out_or_reach_the_limit() {
        let mut only_reviews = GithubWorkItemsConfig::default().queries;
        only_reviews.changes_requested.clear();
        only_reviews.ci_failing.clear();
        only_reviews.assigned.clear();
        only_reviews.ready_to_merge.clear();
        let gh = fake_gh("paging", 250);
        let source = GithubSource::new(GithubWorkItemsConfig {
            gh_path: gh.display().to_string(),
            max_results: 1000,
            queries: only_reviews.clone(),
            ..GithubWorkItemsConfig::default()
        });
        assert_eq!(source.poll().expect("poll").len(), 250);
        let pages = std::fs::read_to_string(gh.with_file_name("pages")).unwrap();
        assert_eq!(pages.lines().collect::<Vec<_>>(), vec!["1", "2", "3"]);

        let limited = GithubSource::new(GithubWorkItemsConfig {
            gh_path: gh.display().to_string(),
            max_results: 120,
            queries: only_reviews,
            ..GithubWorkItemsConfig::default()
        });
        let items = limited.poll().expect("poll");
        let _ = std::fs::remove_dir_all(gh.parent().unwrap());
        assert_eq!(items.len(), 120);
        assert_eq!(items[119].external_id, "o/r#120");
    }

    #[test]
    fn approved_pull_requests_are_ready_to_merge_even_without_a_review_decision() {
        let pull = |number: u64, draft: bool, decision: &str, reviews: &[(&str, &str)]| {
            serde_json::json!({
                "number": number, "title": format!("PR {number}"),
                "url": format!("https://github.com/o/r/pull/{number}"),
                "updatedAt": "2026-09-25T10:00:00Z", "isDraft": draft,
                "author": {"login": "me"}, "repository": {"nameWithOwner": "o/r"},
                "reviewDecision": decision,
                "latestReviews": {"nodes": reviews.iter().map(|(login, state)| serde_json::json!({
                    "state": state, "author": {"login": login}
                })).collect::<Vec<_>>()},
            })
        };
        let response = serde_json::json!({"data": {"search": {"nodes": [
            // Reviews not required: GitHub leaves the decision empty.
            pull(1, false, "", &[("alice", "APPROVED"), ("bob", "APPROVED")]),
            pull(2, false, "APPROVED", &[]),
            pull(3, false, "", &[("alice", "APPROVED"), ("bob", "CHANGES_REQUESTED")]),
            pull(4, true, "APPROVED", &[("alice", "APPROVED")]),
            pull(5, false, "REVIEW_REQUIRED", &[("alice", "APPROVED")]),
            pull(6, false, "", &[("alice", "COMMENTED")]),
        ]}}});
        let items = parse_ready_to_merge(response.to_string().as_bytes()).expect("parses");
        let ids: Vec<&str> = items.iter().map(|item| item.external_id.as_str()).collect();
        assert_eq!(ids, vec!["merge:o/r#1", "merge:o/r#2"]);
        assert_eq!(items[0].context, "#1 ready to merge · o/r");
    }

    fn merge_detail(state: &str) -> GithubDetail {
        GithubDetail {
            merge_state_status: state.into(),
            latest_reviews: vec![GithubReview {
                author: Some(GithubLogin {
                    login: "tony".into(),
                }),
                state: "APPROVED".into(),
                body: String::new(),
            }],
            merge_methods: vec!["SQUASH".into(), "MERGE".into()],
            ..detail(&[("src/a.rs", 3, 1)])
        }
    }

    fn merge_item(detail: &GithubDetail) -> WorkItem {
        WorkItem {
            key: "github:merge:o/r#5".into(),
            external_id: "merge:o/r#5".into(),
            ..work_item("o/r", Some(detail))
        }
    }

    #[test]
    fn clean_pull_request_offers_your_default_merge_with_a_confirmation() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let choices = source.choices(&merge_item(&merge_detail("CLEAN")));
        let ids: Vec<&str> = choices
            .choices
            .iter()
            .map(|choice| choice.choice_id.as_str())
            .collect();
        assert_eq!(ids, vec!["merge_squash", "merge_commit", "github"]);
        assert_eq!(choices.default_choice_id.as_deref(), Some("merge_squash"));
        let squash = &choices.choices[0];
        assert_eq!(squash.action, WorkItemChoiceAction::Perform);
        assert!(squash
            .confirm
            .as_deref()
            .is_some_and(|prompt| prompt.contains("cannot be undone")));
        assert_eq!(
            merge_summary(&merge_detail("CLEAN")),
            "Approved by tony · ready to merge"
        );
    }

    #[test]
    fn blocked_merge_is_disabled_with_the_reason_and_defaults_to_github() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let choices = source.choices(&merge_item(&merge_detail("BEHIND")));
        assert_eq!(
            choices.choices[0].disabled_reason.as_deref(),
            Some("Behind main; update the branch first")
        );
        assert_eq!(choices.default_choice_id.as_deref(), Some("github"));
        assert!(source
            .perform(&merge_item(&merge_detail("BEHIND")), "merge_squash")
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn merging_runs_gh_pr_merge_pinned_to_the_reviewed_commit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-fake-gh-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        let log = dir.join("args");
        std::fs::write(
            &gh,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = GithubSource::new(GithubWorkItemsConfig {
            gh_path: gh.display().to_string(),
            ..GithubWorkItemsConfig::default()
        });
        let result = source.perform(&merge_item(&merge_detail("CLEAN")), "merge_commit");
        let args = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(result.as_deref(), Ok("Merged #5 into main"));
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "pr",
                "merge",
                "5",
                "--repo",
                "o/r",
                "--merge",
                "--match-head-commit",
                "0123456789abcdef"
            ]
        );
    }
}
