//! GitHub pull requests read through the GitHub CLI: review requests for you, and your own
//! pull requests with changes requested.

use std::collections::HashMap;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemChoiceOptionInfo};
use crate::config::{
    BranchWorkflowConfig, GithubRepoConfig, GithubWorkItemsConfig, OnResolvedConfig,
    ReviewRequestedConfig,
};

use super::process::{failure_detail, run_with_timeout};
use super::source::{
    DownloadSpec, ItemChoices, LinkedClone, PreparedItem, ProvisionPlan, SourceItem, StartReminder,
    TicketDetail, WorkItemSource, WorkspaceLayout, WorkspaceSource, WorktreeSpec,
    LINK_CLONE_CHOICE_ID, START_WORK_CHOICE_ID,
};
use super::state::WorkItem;

pub(crate) const SOURCE_ID: &str = "github";
const GH_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Cloning a large repository over a slow link takes a while.
const CLONE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MIN_POLL_SECONDS: u64 = 30;
const MAX_POLL_SECONDS: u64 = 3600;
/// GitHub's search page size and per-query result limits.
const SEARCH_PAGE_SIZE: usize = 100;
const MAX_SEARCH_RESULTS: usize = 1000;
const PUSH_REPLY_CHOICE_ID: &str = "push_reply";
const PUSH_FIX_CHOICE_ID: &str = "push_fix";
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
            Self::ReviewRequested => &[ReviewMode::Review],
            Self::ChangesRequested => &[ReviewMode::Address, ReviewMode::AddressAgent],
            Self::CiFailing => &[ReviewMode::FixChecks, ReviewMode::FixChecksAgent],
            Self::Assigned => &[ReviewMode::StartIssue, ReviewMode::StartIssueAgent],
            Self::Mentioned => &[ReviewMode::ThreadAgent],
            // Merging is carried out by the source; the workspace is for review nits.
            Self::ReadyToMerge => &[ReviewMode::Address, ReviewMode::AddressAgent],
        }
    }
}

pub(crate) struct GithubSource {
    config: GithubWorkItemsConfig,
    /// Review-request blocks, in configuration order.
    review_requested: Vec<ReviewRequestedConfig>,
    /// Defaults used when no block matches a repository.
    fallback: ReviewRequestedConfig,
    branch_fallback: BranchWorkflowConfig,
    /// Your GitHub login, to tell whether an issue is assigned to you.
    viewer: std::sync::Mutex<Option<String>>,
}

/// The workflow settings that apply to one item.
struct Settings<'a> {
    agent: &'a str,
    agent_args: &'a [String],
    tabs: &'a [crate::config::WorkspaceTabConfig],
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
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    assignees: Vec<SearchUser>,
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
    /// Users whose review is currently requested; only fetched for changes-requested items.
    #[serde(default)]
    pub review_requests: Vec<GithubReviewRequest>,
    /// Inline review comments; only fetched for changes-requested and ready-to-merge items.
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
    /// Whether the issue is assigned to you; unknown for pull requests and older details.
    #[serde(default)]
    pub assigned_to_me: Option<bool>,
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
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    labels: Vec<RestLabel>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    assignee: Option<SearchUser>,
    #[serde(default)]
    assignees: Vec<SearchUser>,
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

/// One entry of `gh pr view --json reviewRequests`. Teams come back without a login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubReviewRequest {
    #[serde(default)]
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
    /// REST comment id; 0 in details stored before it was kept.
    #[serde(default)]
    pub id: u64,
    /// Id of the thread's first comment when this comment is a reply.
    #[serde(default)]
    pub in_reply_to: Option<u64>,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub author: String,
    pub body: String,
}

/// One entry of `GET repos/{repo}/pulls/{n}/comments`.
#[derive(Deserialize)]
struct RestReviewComment {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
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

/// A directory name a repository owner or name can safely become: no separators, no
/// parent references.
fn is_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether a Git remote URL points at `repo` (owner/name), whatever the host or protocol:
/// `git@github.com:o/r.git`, `https://github.com/o/r`, `ssh://git@host/o/r.git`.
fn remote_url_names(url: &str, repo: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let mut segments = url.rsplit(['/', ':']);
    let (Some(name), Some(owner)) = (segments.next(), segments.next()) else {
        return false;
    };
    repo.split_once('/').is_some_and(|(want_owner, want_name)| {
        owner.eq_ignore_ascii_case(want_owner) && name.eq_ignore_ascii_case(want_name)
    })
}

/// The remote of the clone at `path` that serves `repo`, if one does.
fn remote_serving(path: &Path, repo: &str) -> Option<String> {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(path)
        .args(["config", "--get-regexp", r"^remote\..*\.url$"]);
    let output = run_with_timeout(command, GH_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(_, url)| remote_url_names(url, repo))
        .and_then(|(key, _)| key.strip_prefix("remote.")?.strip_suffix(".url"))
        .map(str::to_string)
}

/// The clone of `repo` among the directories directly inside `root`, with the remote that
/// serves it. A directory named after the repository is tried first; worktrees, whose `.git`
/// is a file, are skipped so the main checkout is the one linked.
fn find_clone(root: &Path, repo: &str) -> Result<Option<(std::path::PathBuf, String)>, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", root.display())),
    };
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let mut clones: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").is_dir())
        .collect();
    clones.sort_by_key(|path| {
        let named = path
            .file_name()
            .is_some_and(|file| file.to_string_lossy().eq_ignore_ascii_case(name));
        (!named, path.clone())
    });
    Ok(clones
        .into_iter()
        .find_map(|path| remote_serving(&path, repo).map(|remote| (path, remote))))
}

impl GithubSource {
    pub(crate) fn new(config: GithubWorkItemsConfig) -> Self {
        Self {
            review_requested: config.review_requested.clone(),
            fallback: ReviewRequestedConfig::default(),
            branch_fallback: BranchWorkflowConfig::default(),
            config,
            viewer: std::sync::Mutex::new(None),
        }
    }

    /// The review-request workflow for `repo`: the first matching block, else the defaults.
    fn workflow(&self, repo: &str) -> &ReviewRequestedConfig {
        self.review_requested
            .iter()
            .find(|block| block.applies_to(repo))
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
            let config = self.workflow(repo);
            return Settings {
                agent: &config.agent,
                agent_args: &config.agent_args,
                tabs: &config.tabs,
                diff_command: &config.diff_command,
                delete_branch: config.delete_branch,
                on_resolved: config.on_resolved,
            };
        }
        let config = self.branch_workflow(event, repo);
        Settings {
            agent: &config.agent,
            agent_args: &config.agent_args,
            tabs: &config.tabs,
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

    /// Why `mode` cannot be offered for `repo`, if it cannot. `switches` are the ones the
    /// review choice runs with.
    fn mode_unavailable(
        &self,
        mode: ReviewMode,
        switches: ReviewSwitches,
        event: Event,
        repo: &str,
        mapped: bool,
    ) -> Option<String> {
        if mode.checks_out(switches) && !mapped {
            return Some(if self.config.clone_root.trim().is_empty() {
                format!("No local checkout configured for {repo}")
            } else {
                format!("Link or clone {repo} first")
            });
        }
        if mode.needs_agent() && self.settings(event, repo).agent.is_empty() {
            return Some(format!("No agent configured for {repo} {}", event.name()));
        }
        None
    }

    /// Your login, asked once.
    fn viewer_login(&self) -> Result<String, String> {
        if let Some(login) = self.viewer.lock().ok().and_then(|login| login.clone()) {
            return Ok(login);
        }
        let stdout = self.run_gh(&["api", "user", "--jq", ".login"])?;
        let login = String::from_utf8_lossy(&stdout).trim().to_string();
        if login.is_empty() {
            return Err("gh did not say who you are".into());
        }
        if let Ok(mut cached) = self.viewer.lock() {
            *cached = Some(login.clone());
        }
        Ok(login)
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

    /// Like `run_gh`, but a 404 from GitHub becomes `Ok(None)` instead of an error.
    fn run_gh_optional(&self, args: &[&str]) -> Result<Option<Vec<u8>>, String> {
        let mut command = self.gh();
        command.args(args);
        match run_with_timeout(command, GH_TIMEOUT) {
            Ok(output) if output.status.success() => Ok(Some(output.stdout)),
            Ok(output) if String::from_utf8_lossy(&output.stderr).contains("404") => Ok(None),
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

    /// `clone_root`, expanded; `None` while linking is off.
    fn clone_root(&self) -> Option<std::path::PathBuf> {
        let root = self.config.clone_root.trim();
        (!root.is_empty()).then(|| crate::worktree::expand_tilde_absolute_path(root))
    }

    /// Offered while `repo` has no clone but could get one in the clone root.
    fn link_choice(&self, event: Event, repo: &str, mapped: bool) -> Option<WorkItemChoiceInfo> {
        // A review checks the pull request out once its worktree is switched on.
        let worktree = ReviewSwitches {
            worktree: true,
            post: false,
        };
        if mapped
            || self.clone_root().is_none()
            || !event.modes().iter().any(|mode| mode.checks_out(worktree))
        {
            return None;
        }
        Some(WorkItemChoiceInfo {
            choice_id: LINK_CLONE_CHOICE_ID.into(),
            label: format!("Link or clone {repo}"),
            description: Some(format!(
                "Uses its clone in {}, cloning it there if there is none",
                self.config.clone_root.trim()
            )),
            action: WorkItemChoiceAction::Perform,
            disabled_reason: None,
            confirm: None,
            options: Vec::new(),
        })
    }

    /// Clones `repo` into a free directory of `root` named after it.
    fn clone_into(&self, root: &Path, repo: &str) -> Result<std::path::PathBuf, String> {
        let (owner, name) = repo
            .split_once('/')
            .filter(|(owner, name)| is_path_segment(owner) && is_path_segment(name))
            .ok_or_else(|| format!("{repo} is not a repository name Herdr can clone"))?;
        let candidates = [root.join(name), root.join(format!("{owner}-{name}"))];
        let Some(path) = candidates.iter().find(|path| !path.exists()) else {
            return Err(format!(
                "{} and {} exist but are not clones of {repo}",
                candidates[0].display(),
                candidates[1].display()
            ));
        };
        let mut command = self.gh();
        command
            .args(["repo", "clone", repo])
            .arg(path)
            .args(["--", "--quiet"])
            .env("GIT_TERMINAL_PROMPT", "0");
        let failure = match run_with_timeout(command, CLONE_TIMEOUT) {
            Ok(output) if output.status.success() => return Ok(path.clone()),
            // gh ends with "failed to run git"; git's first complaint names the cause, e.g.
            // "Permission denied (publickey)".
            Ok(output) => super::process::first_line(&output.stderr)
                .unwrap_or_else(|| failure_detail(&output)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                "GitHub CLI not found; install gh or set work_items.github.gh_path".into()
            }
            Err(err) => err.to_string(),
        };
        // Only what this clone created: the directory did not exist before it.
        if path.exists() {
            if let Err(err) = std::fs::remove_dir_all(path) {
                warn!(path = %path.display(), %err, "failed to remove a partial clone");
            }
        }
        Err(format!("Cloning {repo} failed: {failure}"))
    }
}

/// At most this many reviewers are named in a status line; the rest are counted.
const NAMED_REVIEWERS: usize = 2;

/// `@a, @b +2`.
fn named_reviewers(logins: &[&str]) -> String {
    let named = logins
        .iter()
        .take(NAMED_REVIEWERS)
        .map(|login| format!("@{login}"))
        .collect::<Vec<_>>()
        .join(", ");
    match logins.len().saturating_sub(NAMED_REVIEWERS) {
        0 => named,
        rest => format!("{named} +{rest}"),
    }
}

/// Logins, other than `viewer`, whose latest review is in `state`.
fn reviewers_in<'a>(
    reviews: &'a [GithubReview],
    state: &str,
    viewer: Option<&str>,
) -> Vec<&'a str> {
    reviews
        .iter()
        .filter(|review| review.state == state)
        .filter_map(|review| review.author.as_ref().map(|author| author.login.as_str()))
        .filter(|login| viewer.is_none_or(|viewer| !login.eq_ignore_ascii_case(viewer)))
        .collect()
}

/// Where an open pull request's reviews stand, yours first, then everyone else's verdict
/// with who gave it, e.g. "awaiting your review · approved by @alice". GitHub only computes
/// a review decision when the repository requires reviews, so without one the latest
/// reviews decide. `viewer` is your login, when known.
fn review_status(
    decision: Option<&str>,
    reviews: &[GithubReview],
    requests: &[GithubReviewRequest],
    viewer: Option<&str>,
) -> String {
    let is_viewer = |login: &str| viewer.is_some_and(|viewer| login.eq_ignore_ascii_case(viewer));
    // A fresh request for your review outweighs the review you gave before it.
    let yours = if requests.iter().any(|request| is_viewer(&request.login)) {
        Some("awaiting your review")
    } else {
        reviews
            .iter()
            .find(|review| {
                review
                    .author
                    .as_ref()
                    .is_some_and(|author| is_viewer(&author.login))
            })
            .and_then(|review| match review.state.as_str() {
                "APPROVED" => Some("you approved"),
                "CHANGES_REQUESTED" => Some("you requested changes"),
                _ => None,
            })
    };
    let decision = decision.filter(|decision| !decision.is_empty());
    let changes_requested = match decision {
        Some(decision) => decision == "CHANGES_REQUESTED",
        None => reviews
            .iter()
            .any(|review| review.state == "CHANGES_REQUESTED"),
    };
    let verdict = |state: &str, label: &str| {
        let others = reviewers_in(reviews, state, viewer);
        if !others.is_empty() {
            Some(format!("{label} by {}", named_reviewers(&others)))
        } else if yours.is_some() && reviews.iter().any(|review| review.state == state) {
            // Only your own review gives this verdict, and `yours` already says so.
            None
        } else {
            Some(label.to_string())
        }
    };
    let others = if changes_requested {
        verdict("CHANGES_REQUESTED", "changes requested")
    } else if is_approved(decision, reviews) {
        verdict("APPROVED", "approved")
    } else if yours == Some("awaiting your review") {
        None
    } else {
        Some("awaiting review".to_string())
    };
    match (yours, others) {
        (Some(yours), Some(others)) => format!("{yours} · {others}"),
        (Some(yours), None) => yours.to_string(),
        (None, Some(others)) => others,
        (None, None) => "awaiting review".to_string(),
    }
}

/// What GitHub says about a pull request's reviews, for its one-line status.
struct ReviewState<'a> {
    decision: Option<&'a str>,
    reviews: &'a [GithubReview],
    requests: &'a [GithubReviewRequest],
    viewer: Option<&'a str>,
}

/// A pull request's state in one line, e.g. "draft", "approved by @bob · CI failing",
/// "merged".
fn pull_request_status(
    state: &str,
    is_draft: bool,
    review: ReviewState<'_>,
    (failing, running): (bool, bool),
) -> String {
    if !state.eq_ignore_ascii_case("open") {
        return state.to_lowercase();
    }
    let review = if is_draft {
        "draft".to_string()
    } else {
        review_status(
            review.decision,
            review.reviews,
            review.requests,
            review.viewer,
        )
    };
    match (failing, running) {
        (true, _) => format!("{review} · CI failing"),
        (false, true) => format!("{review} · checks running"),
        (false, false) => review,
    }
}

/// The first pull request of a `gh pr list --json` answer, with its state in one line.
/// `viewer` is your login, when known.
fn parse_branch_pull_request(
    bytes: &[u8],
    source_id: &str,
    repo: &str,
    viewer: Option<&str>,
) -> Result<Option<crate::api::schema::WorkItemPullRequestInfo>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Listed {
        number: u64,
        url: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        is_draft: bool,
        #[serde(default)]
        review_decision: Option<String>,
        #[serde(default)]
        latest_reviews: Vec<GithubReview>,
        #[serde(default)]
        review_requests: Vec<GithubReviewRequest>,
        #[serde(default)]
        status_check_rollup: Vec<Check>,
    }
    #[derive(Deserialize)]
    struct Check {
        #[serde(default)]
        conclusion: Option<String>,
        /// Commit statuses report `state`; check runs report `status` and `conclusion`.
        #[serde(default)]
        state: Option<String>,
        #[serde(default)]
        status: Option<String>,
    }
    let listed: Vec<Listed> =
        serde_json::from_slice(bytes).map_err(|err| format!("unexpected gh output: {err}"))?;
    let Some(pull) = listed.into_iter().next() else {
        return Ok(None);
    };
    let is = |value: &Option<String>, wanted: &[&str]| {
        value
            .as_deref()
            .is_some_and(|value| wanted.iter().any(|w| value.eq_ignore_ascii_case(w)))
    };
    let failing = pull.status_check_rollup.iter().any(|check| {
        is(
            &check.conclusion,
            &[
                "FAILURE",
                "TIMED_OUT",
                "CANCELLED",
                "ACTION_REQUIRED",
                "STARTUP_FAILURE",
            ],
        ) || is(&check.state, &["FAILURE", "ERROR"])
    });
    let running = pull.status_check_rollup.iter().any(|check| {
        is(
            &check.status,
            &["QUEUED", "IN_PROGRESS", "PENDING", "WAITING"],
        ) || is(&check.state, &["PENDING", "EXPECTED"])
    });
    let open = pull.state.eq_ignore_ascii_case("open");
    Ok(Some(crate::api::schema::WorkItemPullRequestInfo {
        source_id: source_id.to_string(),
        repo: repo.to_string(),
        number: pull.number,
        status: pull_request_status(
            &pull.state,
            pull.is_draft,
            ReviewState {
                decision: pull.review_decision.as_deref(),
                reviews: &pull.latest_reviews,
                requests: &pull.review_requests,
                viewer,
            },
            (failing, running),
        ),
        url: pull.url,
        is_draft: open && pull.is_draft,
    }))
}

/// One aliased GraphQL query reading many pull requests' state at once.
/// Teams requested for review come back without a login.
const PULL_REQUEST_STATUS_FIELDS: &str = "number url state isDraft reviewDecision \
    latestReviews(first: 50) { nodes { state author { login } } } \
    reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } } \
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

/// Pull requests read per GraphQL query, keeping each well under GitHub's node limits.
const PULL_REQUEST_STATUS_BATCH: usize = 40;

/// The query and its `gh api graphql` field arguments for `pulls`, aliased `p0`, `p1`, ….
fn pull_request_status_query(pulls: &[(String, u64)]) -> (String, Vec<String>) {
    let mut variables = Vec::new();
    let mut selections = String::new();
    let mut args = Vec::new();
    for (index, (repo, number)) in pulls.iter().enumerate() {
        let (owner, name) = repo.split_once('/').unwrap_or((repo.as_str(), ""));
        variables.push(format!(
            "$o{index}: String!, $n{index}: String!, $p{index}: Int!"
        ));
        selections.push_str(&format!(
            " p{index}: repository(owner: $o{index}, name: $n{index}) \
             {{ pullRequest(number: $p{index}) {{ {PULL_REQUEST_STATUS_FIELDS} }} }}"
        ));
        args.extend([
            "-f".to_string(),
            format!("o{index}={owner}"),
            "-f".to_string(),
            format!("n{index}={name}"),
            "-F".to_string(),
            format!("p{index}={number}"),
        ]);
    }
    let query = format!("query({}) {{{selections} }}", variables.join(", "));
    (query, args)
}

/// Each of `pulls` from a `pull_request_status_query` answer, `None` where GitHub could not
/// resolve it, e.g. a repository you lost access to. `viewer` is your login, when known.
fn parse_pull_request_statuses(
    bytes: &[u8],
    source_id: &str,
    pulls: &[(String, u64)],
    viewer: Option<&str>,
) -> Result<Vec<Option<crate::api::schema::WorkItemPullRequestInfo>>, String> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        data: Option<HashMap<String, Option<Repository>>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        #[serde(default)]
        pull_request: Option<Pull>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pull {
        number: u64,
        url: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        is_draft: bool,
        #[serde(default)]
        review_decision: Option<String>,
        #[serde(default)]
        latest_reviews: Option<GraphqlReviews>,
        #[serde(default)]
        review_requests: Option<ReviewRequests>,
        #[serde(default)]
        commits: Option<Commits>,
    }
    #[derive(Deserialize)]
    struct ReviewRequests {
        #[serde(default)]
        nodes: Vec<Option<ReviewRequest>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ReviewRequest {
        #[serde(default)]
        requested_reviewer: Option<GithubReviewRequest>,
    }
    #[derive(Deserialize)]
    struct Commits {
        #[serde(default)]
        nodes: Vec<Option<CommitNode>>,
    }
    #[derive(Deserialize)]
    struct CommitNode {
        commit: Commit,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Commit {
        #[serde(default)]
        status_check_rollup: Option<Rollup>,
    }
    #[derive(Deserialize)]
    struct Rollup {
        #[serde(default)]
        state: String,
    }
    let response: Response = serde_json::from_slice(bytes)
        .map_err(|err| format!("unexpected gh api graphql output: {err}"))?;
    let Some(mut data) = response.data else {
        return Err("gh api graphql returned no data".into());
    };
    Ok(pulls
        .iter()
        .enumerate()
        .map(|(index, (repo, _))| {
            let pull = data.remove(&format!("p{index}")).flatten()?.pull_request?;
            let reviews: Vec<GithubReview> = pull
                .latest_reviews
                .iter()
                .flat_map(|reviews| reviews.nodes.iter().flatten().cloned())
                .collect();
            let requests: Vec<GithubReviewRequest> = pull
                .review_requests
                .into_iter()
                .flat_map(|requests| requests.nodes.into_iter().flatten())
                .filter_map(|request| request.requested_reviewer)
                .collect();
            let rollup = pull
                .commits
                .and_then(|commits| commits.nodes.into_iter().flatten().last())
                .and_then(|node| node.commit.status_check_rollup)
                .map(|rollup| rollup.state)
                .unwrap_or_default();
            let checks = (
                matches!(rollup.as_str(), "FAILURE" | "ERROR"),
                matches!(rollup.as_str(), "PENDING" | "EXPECTED"),
            );
            let open = pull.state.eq_ignore_ascii_case("open");
            Some(crate::api::schema::WorkItemPullRequestInfo {
                source_id: source_id.to_string(),
                repo: repo.clone(),
                number: pull.number,
                status: pull_request_status(
                    &pull.state,
                    pull.is_draft,
                    ReviewState {
                        decision: pull.review_decision.as_deref(),
                        reviews: &reviews,
                        requests: &requests,
                        viewer,
                    },
                    checks,
                ),
                url: pull.url,
                is_draft: open && pull.is_draft,
            })
        })
        .collect())
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

/// The choice the dialog highlights: a review is done by the agent, and your own work is done
/// locally whenever there is a checkout. Mentions have none, and the default of a pull request
/// that is ready to merge is decided by `merge_choices`.
fn default_choice(event: Event, mapped: bool) -> Option<&'static str> {
    match event {
        Event::ReviewRequested => Some(ReviewMode::Review.choice_id()),
        Event::Mentioned | Event::ReadyToMerge => None,
        _ if !mapped => None,
        Event::ChangesRequested => Some(ReviewMode::Address.choice_id()),
        Event::CiFailing => Some(ReviewMode::FixChecks.choice_id()),
        Event::Assigned => Some(ReviewMode::StartIssue.choice_id()),
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
    /// The agent reviews the pull request and reports back. The choice's switches decide how:
    /// see `ReviewSwitches`.
    Review,
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

/// Id of the review choice's switch that checks the pull request out in a worktree.
const WORKTREE_OPTION_ID: &str = "worktree";
/// Id of the review choice's switch that has the agent post its review on GitHub.
const POST_OPTION_ID: &str = "post";

/// The switches of the review choice, which make it one of four ways to review a pull
/// request. Both are off for every other choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReviewSwitches {
    /// The agent works in a worktree of the pull request head, with the user's tools, rather
    /// than from a downloaded diff.
    worktree: bool,
    /// The agent posts its review on GitHub as a comment, after showing it to the user.
    post: bool,
}

impl ReviewSwitches {
    /// The switches `options`, the ids switched on, name.
    fn from_options(options: &[String]) -> Self {
        let on = |id: &str| options.iter().any(|option| option == id);
        Self {
            worktree: on(WORKTREE_OPTION_ID),
            post: on(POST_OPTION_ID),
        }
    }
}

impl ReviewMode {
    const ALL: [Self; 8] = [
        Self::Review,
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
            Self::Review => "review",
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
            Self::Review => "Review",
            Self::Address | Self::FixChecks | Self::StartIssue => "Work on it locally",
            Self::AddressAgent => "Ask agent to address the feedback",
            Self::FixChecksAgent => "Ask agent to fix the checks",
            Self::StartIssueAgent => "Ask agent to implement it",
            Self::ThreadAgent => "Ask agent to draft a reply",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Review => "The agent reviews the pull request and reports back to you",
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

    /// The switches the user sets before running the choice. A worktree is only on offer for
    /// a repository that has a local clone (`mapped`), and then on unless switched off, as
    /// the choice that review defaulted to before the switches always had one. Posting on
    /// GitHub is visible to others, so it is off unless switched on.
    fn options(self, mapped: bool) -> Vec<WorkItemChoiceOptionInfo> {
        if self != Self::Review {
            return Vec::new();
        }
        let mut options = Vec::new();
        if mapped {
            options.push(WorkItemChoiceOptionInfo {
                option_id: WORKTREE_OPTION_ID.into(),
                label: "Create worktree".into(),
                description: Some(
                    "Check the pull request out in a worktree, with your tools open. Off: the \
                     agent reads a downloaded diff."
                        .into(),
                ),
                default: true,
            });
        }
        options.push(WorkItemChoiceOptionInfo {
            option_id: POST_OPTION_ID.into(),
            label: "Post to GitHub".into(),
            description: Some(
                "The agent shows you its review, then comments it on the pull request. It never \
                 approves or requests changes."
                    .into(),
            ),
            default: false,
        });
        options
    }

    /// Whether the agent works in a checkout of the repository. Only the review choice's
    /// worktree switch makes that a choice.
    fn checks_out(self, switches: ReviewSwitches) -> bool {
        match self {
            Self::Review => switches.worktree,
            Self::ThreadAgent => false,
            _ => true,
        }
    }

    fn needs_agent(self) -> bool {
        !matches!(self, Self::Address | Self::FixChecks | Self::StartIssue)
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

/// Reviews with feedback, then inline comments, as brief lines. Changes-requested items
/// list the reviews requesting changes; ready-to-merge items every latest review with text.
/// Other reviews are left to the full threads the brief points at.
fn feedback_list(detail: &GithubDetail, event: Event) -> String {
    let reviews: Vec<&GithubReview> = if event == Event::ReadyToMerge {
        detail
            .latest_reviews
            .iter()
            .filter(|review| !review.body.trim().is_empty())
            .collect()
    } else {
        detail
            .reviews
            .iter()
            .filter(|review| review.state == "CHANGES_REQUESTED")
            .collect()
    };
    let mut lines: Vec<String> = reviews
        .into_iter()
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
        let thread = if comment.id == 0 {
            String::new()
        } else {
            format!("[thread {}] ", comment.in_reply_to.unwrap_or(comment.id))
        };
        lines.push(format!(
            "- {thread}{location} @{}: {}",
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

/// Logins whose latest review requests changes, in first-seen order. Comment-only and pending
/// reviews do not change a reviewer's verdict, as in GitHub's review decision.
fn changes_requesters(detail: &GithubDetail) -> Vec<String> {
    let mut latest: Vec<(&str, &str)> = Vec::new();
    for review in &detail.reviews {
        if matches!(review.state.as_str(), "COMMENTED" | "PENDING") {
            continue;
        }
        let Some(login) = review.author.as_ref().map(|author| author.login.as_str()) else {
            continue;
        };
        match latest.iter_mut().find(|(seen, _)| *seen == login) {
            Some(entry) => entry.1 = review.state.as_str(),
            None => latest.push((login, review.state.as_str())),
        }
    }
    latest
        .into_iter()
        .filter(|(_, state)| *state == "CHANGES_REQUESTED")
        .map(|(login, _)| login.to_string())
        .collect()
}

/// The reviewers requesting changes, when every one of them has been asked to review again.
fn waiting_for_rereview(detail: &GithubDetail) -> Option<Vec<String>> {
    let requesters = changes_requesters(detail);
    let all_requested = requesters.iter().all(|login| {
        detail
            .review_requests
            .iter()
            .any(|request| !request.login.is_empty() && request.login == *login)
    });
    (!requesters.is_empty() && all_requested).then_some(requesters)
}

/// `@a, @b`.
fn at_logins(logins: &[String]) -> String {
    logins
        .iter()
        .map(|login| format!("@{login}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn changes_brief(repo: &str, item: &WorkItem, detail: &GithubDetail, mode: ReviewMode) -> String {
    let number = detail.number;
    let head = &detail.head_ref_name;
    let event = Event::of(&item.external_id);
    let heading = if event == Event::ReadyToMerge {
        format!("Your GitHub pull request {repo}#{number} is approved; reviewers left comments")
    } else {
        format!("Reviewers requested changes on your GitHub pull request {repo}#{number}")
    };
    let instructions = if mode == ReviewMode::AddressAgent {
        "Address each point now: make the changes and run the relevant tests, then commit \
         them on this branch. Do not push, reply or comment on GitHub yet. Report what you \
         changed and any point you disagree with."
    } else {
        "Summarise what the reviewers asked for, propose how to address each point and wait \
         for my instructions before changing anything."
    };
    format!(
        "{heading}: {title}\n\
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
        feedback = feedback_list(detail, event),
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
        "Fix the failures now and rerun the failing tests locally where you can, then commit \
         the fix on this branch. Do not push or rerun CI. Report what you changed."
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

/// Follow-up asking the agent in the item's workspace to send its changes back to the
/// reviewers. The agent does the GitHub writes; the user approves each in its pane.
fn push_reply_brief(repo: &str, detail: &GithubDetail, event: Event) -> String {
    let number = detail.number;
    let requesters = changes_requesters(detail);
    let rerequest = if requesters.is_empty() {
        "Nobody currently requests changes, so do not re-request review.".to_string()
    } else {
        let reviewers: String = requesters
            .iter()
            // Quoted: zsh treats an unquoted `[]` as a glob.
            .map(|login| format!(" -f 'reviewers[]={login}'"))
            .collect();
        format!(
            "Re-request review from {}: `gh api --method POST \
             repos/{repo}/pulls/{number}/requested_reviewers{reviewers}`",
            at_logins(&requesters)
        )
    };
    format!(
        "Send your changes for {repo}#{number} back to the reviewers now:\n\
         1. Commit anything not yet committed on this branch ({head}).\n\
         2. Push with a plain `git push`. Never force-push. If GitHub rejects the push because \
         it has newer commits, stop and tell me.\n\
         3. Reply once in each review thread you addressed, saying what changed or, where you \
         disagree, why: `gh api --method POST \
         repos/{repo}/pulls/{number}/comments/<thread id>/replies -f body=<reply>`. The thread \
         ids are below; `gh api repos/{repo}/pulls/{number}/comments` lists them all. Do not \
         resolve threads and do not post any other comment.\n\
         4. {rerequest}\n\
         Then report what you pushed and posted.\n\
         \n\
         Review feedback:\n\
         {feedback}",
        head = detail.head_ref_name,
        feedback = feedback_list(detail, event),
    )
}

/// Follow-up asking the agent in the item's workspace to push its fix for failing checks.
fn push_fix_brief(repo: &str, detail: &GithubDetail) -> String {
    format!(
        "Push your fix for the failing checks on {repo}#{number} now: commit anything not yet \
         committed on this branch ({head}), then push with a plain `git push`. Never \
         force-push. If GitHub rejects the push because it has newer commits, stop and tell \
         me. Do not rerun CI or comment on GitHub. Report what you pushed.",
        number = detail.number,
        head = detail.head_ref_name,
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

/// Brief for the shared "Pick next" discovery workspace: read-only investigation of the
/// tracker, ending in a recommendation the user confirms before anything is added or chosen.
fn pick_next_brief(repos: &[GithubRepoConfig], context: &str) -> String {
    let clones = if repos.is_empty() {
        "(no repositories are mapped to a local clone; use gh directly for detail)".to_string()
    } else {
        repos
            .iter()
            .map(|repo| format!("- {} ({}, remote {})", repo.path, repo.name, repo.remote))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let queries = if repos.is_empty() {
        "  herdr work-item search github \"is:open is:pr author:@me\"".to_string()
    } else {
        repos
            .iter()
            .map(|repo| {
                format!(
                    "  herdr work-item search github \"repo:{} is:open\"",
                    repo.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let context_line = if context.trim().is_empty() {
        String::new()
    } else {
        format!("\nWhat I'm after: {}\n", context.trim())
    };
    format!(
        "Help me pick what to work on next.\n\
         {context_line}\n\
         Read the Herdr skill first (`herdr --skill`) for the general agent workflow; it does \
         not know this fork's work-item commands, so use these instead:\n\
         - `herdr work-item search github \"<query>\"` — GitHub search syntax, e.g.:\n\
         {queries}\n\
         - `herdr work-item show github <owner>/<repo>#<number>` — one ticket's full detail.\n\
         - `herdr work-item list` — the current inbox, with each item's id and choice ids.\n\
         - `herdr work-item add github <owner>/<repo>#<number>` — brings a ticket into the \
         inbox (only after I confirm, see below).\n\
         - `herdr work-item choose <item-id> <choice-id>` — acts on an inbox item (only after \
         I confirm; take the choice id from `herdr work-item list`, never guess it).\n\
         \n\
         This is read-only investigation: only `search`/`show`, and reading these local clones \
         with `git -C <path> ...` to see who else is working on related code and spot overlap:\n\
         {clones}\n\
         Make no tracker writes and no code changes.\n\
         \n\
         Recommend one pick and up to two alternatives. For each: why now, who else works on \
         related tickets, and any overlap risk. Then stop and ask me which one, if any.\n\
         \n\
         Only once I confirm a pick in this chat: run `herdr work-item add github <key>`, then \
         `herdr work-item choose <item-id> <choice-id>` defaulting to the local choice (an \
         agent-led one only if I ask); take ids from `herdr work-item list`, never guess them. \
         Do not assign the ticket to me or move it — I'll be reminded about that separately.",
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

fn brief(
    repo: &str,
    item: &WorkItem,
    detail: &GithubDetail,
    mode: ReviewMode,
    switches: ReviewSwitches,
) -> String {
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
        ReviewMode::Review => match (switches.worktree, switches.post) {
            (true, false) => format!(
                "This directory is a worktree checked out at the pull request head.\n\
                 Start reviewing now: read the diff (`git diff origin/{base}...HEAD`) and the surrounding code. {review}\n\
                 Report the findings to me here. Do not modify files, commit, or post anything to GitHub."
            ),
            (true, true) => format!(
                "This directory is a worktree checked out at the pull request head.\n\
                 Start reviewing now: read the diff (`git diff origin/{base}...HEAD`) and the surrounding code. {review}\n\
                 When you are done, show me the findings and post them as one review comment with \
                 `gh pr review {number} --repo {repo} --comment --body-file <file>`. Write that \
                 file outside this worktree, for example under the system temp directory \
                 (`mktemp`), so the worktree stays clean and can be removed. \
                 Only comment: never approve or request changes. Do not modify files or commit."
            ),
            (false, false) => format!(
                "{gh_context}\n\
                 Start reviewing now. {review}\n\
                 Report the findings to me here. Do not post anything to GitHub."
            ),
            (false, true) => format!(
                "{gh_context}\n\
                 Start reviewing now. {review}\n\
                 When you are done, show me the findings and post them as one review comment with \
                 `gh pr review {number} --repo {repo} --comment --body-file <file>`. \
                 Only comment: never approve or request changes."
            ),
        },
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
                tracker_state: None,
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
            // Pull request items show their state on a status line of their own.
            if item.draft == Some(true) && !event.is_pull_request_event() {
                context.push_str(" · draft");
            }
            Some(SourceItem {
                external_id: format!("{}{repo}#{}", event.id_prefix(), item.number),
                title: item.title,
                context,
                author: item.user.map(|user| user.login),
                url: item.html_url,
                updated_at: item.updated_at,
                tracker_state: None,
            })
        })
        .collect())
}

/// Raw search results as tickets, independent of any configured query's event mapping.
fn parse_ticket_search(
    bytes: &[u8],
) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String> {
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
            })?;
            let merged = item
                .pull_request
                .as_ref()
                .and_then(|pull_request| pull_request.get("merged_at"))
                .is_some_and(|value| !value.is_null());
            let state = item.state.unwrap_or_default();
            let status = if merged {
                "merged".to_string()
            } else {
                state.clone()
            };
            let done = merged || state.eq_ignore_ascii_case("closed");
            Some(crate::api::schema::WorkItemTicketInfo {
                key: format!("{repo}#{}", item.number),
                title: item.title,
                status,
                done,
                assignee: item.assignees.first().map(|user| user.login.clone()),
                updated_at: item.updated_at,
                url: item.html_url,
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
                waiting: false,
                detail: None,
                summary: None,
                error: Some(format!("unrecognised pull request id {}", item.external_id)),
                done: false,
            };
        };
        let event = Event::of(&item.external_id);
        if !event.is_pull_request_event() {
            return self.prepare_issue(event, repo, number);
        }
        let number_arg = number.to_string();
        let fields = match event {
            Event::ChangesRequested => {
                format!("{DETAIL_FIELDS},isCrossRepository,reviews,reviewRequests")
            }
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
                    waiting: false,
                    detail: None,
                    summary: None,
                    error: Some(error),
                    done: false,
                }
            }
        };
        let mut errors = Vec::new();
        if matches!(event, Event::ChangesRequested | Event::ReadyToMerge) {
            match self.inline_comments(repo, number) {
                Ok(comments) => detail.inline_comments = comments,
                Err(error) => errors.push(format!("inline comments unavailable: {error}")),
            }
        }
        match event {
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
        let waiting = (event == Event::ChangesRequested)
            .then(|| waiting_for_rereview(&detail))
            .flatten();
        let summary = match event {
            Event::ChangesRequested => match &waiting {
                Some(logins) => format!("Waiting for re-review from {}", at_logins(logins)),
                None => {
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
            },
            Event::ReadyToMerge => merge_summary(&detail, item.author.as_deref()),
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
            waiting: waiting.is_some(),
            done: false,
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        if item.is_pick_next {
            return ItemChoices {
                choices: Vec::new(),
                default_choice_id: None,
            };
        }
        let event = Event::of(&item.external_id);
        let repo = parse_external_id(&item.external_id)
            .map(|(repo, _)| repo)
            .unwrap_or(&item.external_id);
        let mapped = self.repo(repo).is_some();
        let mut choices: Vec<WorkItemChoiceInfo> = event
            .modes()
            .iter()
            .filter(|&&mode| !superseded_by_pull_request(item, mode))
            .map(|&mode| WorkItemChoiceInfo {
                choice_id: mode.choice_id().into(),
                label: mode.label().into(),
                description: Some(mode.description().into()),
                action: WorkItemChoiceAction::ProvisionWorkspace,
                // Whether a worktree is needed depends on the switches, checked when it runs.
                disabled_reason: self.mode_unavailable(
                    mode,
                    ReviewSwitches::default(),
                    event,
                    repo,
                    mapped,
                ),
                confirm: None,
                options: mode.options(mapped),
            })
            .collect();
        if let Some(link) = self.link_choice(event, repo, mapped) {
            choices.insert(0, link);
        }
        let follow_up = match event {
            Event::ChangesRequested => Some((
                PUSH_REPLY_CHOICE_ID,
                "Ask agent to push and reply",
                "The agent commits, pushes, replies in each thread and re-requests review; you \
                 approve each step in its pane",
            )),
            Event::ReadyToMerge => Some((
                PUSH_REPLY_CHOICE_ID,
                "Ask agent to push and reply",
                "The agent commits, pushes and replies in each thread; you approve each step \
                 in its pane",
            )),
            Event::CiFailing => Some((
                PUSH_FIX_CHOICE_ID,
                "Ask agent to push the fix",
                "The agent commits and pushes its fix; you approve the push in its pane",
            )),
            _ => None,
        };
        if let Some((choice_id, label, description)) = follow_up {
            choices.push(WorkItemChoiceInfo {
                choice_id: choice_id.into(),
                label: label.into(),
                description: Some(description.into()),
                action: WorkItemChoiceAction::BriefAgent,
                disabled_reason: item
                    .workspace_id
                    .is_none()
                    .then(|| "Work on it locally first".into()),
                confirm: None,
                options: Vec::new(),
            });
        }
        if event == Event::ReadyToMerge {
            return merge_choices(item_detail::<GithubDetail>(item).as_ref(), choices);
        }
        // The default is a choice that is offered and can run. A review without an agent
        // configured, a start mode the pull request took away, or an item with nothing worth
        // defaulting to has none.
        let default_choice_id = default_choice(event, mapped)
            .filter(|default| {
                choices
                    .iter()
                    .any(|choice| choice.choice_id == *default && choice.disabled_reason.is_none())
            })
            .map(str::to_string);
        ItemChoices {
            choices,
            default_choice_id,
        }
    }

    fn provision_plan(
        &self,
        item: &WorkItem,
        choice_id: &str,
        options: &[String],
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        let event = Event::of(&item.external_id);
        let mode = ReviewMode::from_choice_id(choice_id)
            .filter(|mode| event.modes().contains(mode))
            .ok_or_else(|| format!("choice {choice_id} does not provision a workspace"))?;
        let switches = ReviewSwitches::from_options(options);
        let (repo_name, number) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised GitHub id {}", item.external_id))?;
        let mapped = self.repo(repo_name);
        if let Some(reason) =
            self.mode_unavailable(mode, switches, event, repo_name, mapped.is_some())
        {
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
            tabs: settings.tabs.to_vec(),
            diff_command: settings.diff_command.to_string(),
        };
        let checkout = mapped.filter(|_| mode.checks_out(switches));
        let (source, brief) = if event.is_pull_request_event() {
            let detail = item_detail::<GithubDetail>(item).ok_or_else(not_ready)?;
            let source = match (checkout, event) {
                (Some(repo), Event::ReviewRequested) => {
                    let (base_refspec, base) = review_base(repo, &detail.base_ref_name);
                    for tab in &mut layout.tabs {
                        tab.command = tab.command.replace("{base}", &base);
                        tab.fallback = tab.fallback.replace("{base}", &base);
                    }
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
            (source, brief(repo_name, item, &detail, mode, switches))
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

    fn start_reminder(&self, item: &WorkItem) -> Option<StartReminder> {
        let detail = item_detail::<GithubIssueDetail>(item)?;
        // Issues only: a pull request has no assignee you need to become.
        if Event::of(&item.external_id).is_pull_request_event()
            || detail.is_pull_request
            || detail.assigned_to_me != Some(false)
        {
            return None;
        }
        Some(StartReminder {
            message: "You're working on this, but it isn't assigned to you".into(),
            choice: WorkItemChoiceInfo {
                choice_id: START_WORK_CHOICE_ID.into(),
                label: "Assign to me".into(),
                description: Some("Adds you as an assignee on GitHub".into()),
                action: WorkItemChoiceAction::Perform,
                disabled_reason: None,
                confirm: None,
                options: Vec::new(),
            },
        })
    }

    fn perform(&self, item: &WorkItem, choice_id: &str) -> Result<String, String> {
        if choice_id == START_WORK_CHOICE_ID {
            let (repo, number) = parse_external_id(&item.external_id)
                .ok_or_else(|| format!("unrecognised issue id {}", item.external_id))?;
            let number_arg = number.to_string();
            self.run_gh(&[
                "issue",
                "edit",
                number_arg.as_str(),
                "--repo",
                repo,
                "--add-assignee",
                "@me",
            ])?;
            return Ok(format!("#{number} is assigned to you"));
        }
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

    fn link_clone(&self, item: &WorkItem) -> Result<(LinkedClone, String), String> {
        let (repo, _) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised GitHub id {}", item.external_id))?;
        if let Some(mapped) = self.repo(repo) {
            return Err(format!("{repo} already has its clone at {}", mapped.path));
        }
        let root = self
            .clone_root()
            .ok_or("Set work_items.github.clone_root to link clones")?;
        let linked = |path: std::path::PathBuf, remote: String| LinkedClone {
            source_id: SOURCE_ID.into(),
            name: repo.to_string(),
            path,
            remote,
        };
        if let Some((path, remote)) = find_clone(&root, repo)? {
            let message = format!("Linked {repo} to {}", path.display());
            return Ok((linked(path, remote), message));
        }
        std::fs::create_dir_all(&root)
            .map_err(|err| format!("cannot create {}: {err}", root.display()))?;
        let path = self.clone_into(&root, repo)?;
        // gh names the cloned repository origin; a fork also gets its parent as upstream.
        let remote = remote_serving(&path, repo).unwrap_or_else(|| "origin".into());
        let message = format!("Cloned {repo} into {}", path.display());
        Ok((linked(path, remote), message))
    }

    fn find_pull_request(
        &self,
        repo_root: &Path,
        branch: &str,
    ) -> Result<Option<crate::api::schema::WorkItemPullRequestInfo>, String> {
        let wanted = crate::worktree::canonical_or_original(repo_root);
        let Some(repo) = self.config.repos.iter().find(|repo| {
            crate::worktree::canonical_or_original(&crate::worktree::expand_tilde_absolute_path(
                &repo.path,
            )) == wanted
        }) else {
            return Ok(None);
        };
        let output = self.run_gh(&[
            "pr",
            "list",
            "--repo",
            &repo.name,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number,url,state,isDraft,reviewDecision,latestReviews,reviewRequests,statusCheckRollup",
        ])?;
        let viewer = self.viewer_login().ok();
        parse_branch_pull_request(&output, self.id(), &repo.name, viewer.as_deref())
    }

    fn mark_pull_request_ready(
        &self,
        pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Result<String, String> {
        let number = pull_request.number.to_string();
        self.run_gh(&["pr", "ready", &number, "--repo", &pull_request.repo])?;
        Ok(format!("#{number} is ready for review"))
    }

    fn pull_request_of(&self, item: &WorkItem) -> Option<(String, u64)> {
        if !Event::of(&item.external_id).is_pull_request_event() {
            return None;
        }
        parse_external_id(&item.external_id).map(|(repo, number)| (repo.to_string(), number))
    }

    fn pull_request_statuses(
        &self,
        pulls: &[(String, u64)],
    ) -> Result<Vec<Option<crate::api::schema::WorkItemPullRequestInfo>>, String> {
        let mut statuses = Vec::with_capacity(pulls.len());
        if pulls.is_empty() {
            return Ok(statuses);
        }
        // Without your login the statuses still read, only not from your side.
        let viewer = self.viewer_login().ok();
        for batch in pulls.chunks(PULL_REQUEST_STATUS_BATCH) {
            let (query, fields) = pull_request_status_query(batch);
            let query_arg = format!("query={query}");
            let mut args = vec!["api", "graphql", "-f", query_arg.as_str()];
            args.extend(fields.iter().map(String::as_str));
            let mut command = self.gh();
            command.args(&args);
            // GitHub answers what it can resolve and exits non-zero for the rest, e.g. a
            // pull request in a repository you lost access to; keep the ones it resolved.
            let stdout = match run_with_timeout(command, GH_TIMEOUT) {
                Ok(output) if output.status.success() || !output.stdout.is_empty() => output.stdout,
                Ok(output) => return Err(gh_error(&output)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(
                        "GitHub CLI not found; install gh or set work_items.github.gh_path".into(),
                    )
                }
                Err(err) => return Err(format!("gh failed: {err}")),
            };
            statuses.extend(parse_pull_request_statuses(
                &stdout,
                self.id(),
                batch,
                viewer.as_deref(),
            )?);
        }
        Ok(statuses)
    }

    fn follow_up_brief(&self, item: &WorkItem, choice_id: &str) -> Result<String, String> {
        let event = Event::of(&item.external_id);
        let briefs = matches!(
            (event, choice_id),
            (
                Event::ChangesRequested | Event::ReadyToMerge,
                PUSH_REPLY_CHOICE_ID
            ) | (Event::CiFailing, PUSH_FIX_CHOICE_ID)
        );
        let repo = parse_external_id(&item.external_id)
            .filter(|_| briefs)
            .map(|(repo, _)| repo)
            .ok_or_else(|| format!("choice {choice_id} does not brief an agent"))?;
        let detail = item_detail::<GithubDetail>(item)
            .ok_or("details are not available yet; try again shortly")?;
        Ok(if choice_id == PUSH_REPLY_CHOICE_ID {
            push_reply_brief(repo, &detail, event)
        } else {
            push_fix_brief(repo, &detail)
        })
    }

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        let title = Event::of(&item.external_id).arrival_title();
        (
            title.into(),
            Some(format!("{} · {}", item.context, item.title)),
        )
    }

    fn tracker_need(&self, item: &WorkItem) -> Option<super::attention::Need> {
        use crate::api::schema::AttentionKind;
        let event = Event::of(&item.external_id);
        let kind = match event {
            Event::ReviewRequested | Event::Assigned | Event::Mentioned => AttentionKind::New,
            Event::ChangesRequested => AttentionKind::ChangesRequested,
            Event::CiFailing => AttentionKind::ChecksFailing,
            Event::ReadyToMerge => AttentionKind::ReadyToMerge,
        };
        Some(super::attention::Need::new(kind, event.arrival_title()))
    }

    fn search(&self, query: &str) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String> {
        let limit = self
            .config
            .max_results
            .clamp(1, MAX_SEARCH_RESULTS)
            .min(SEARCH_PAGE_SIZE);
        let query_arg = format!("q={query}");
        let per_page_arg = format!("per_page={limit}");
        // Single page: an on-demand lookup, not the continuous poll.
        let stdout = self.run_gh(&[
            "api",
            "--method",
            "GET",
            "search/issues",
            "-f",
            &query_arg,
            "-f",
            &per_page_arg,
            "-f",
            "sort=updated",
            "-f",
            "order=desc",
        ])?;
        parse_ticket_search(&stdout)
    }

    fn fetch(&self, key: &str) -> Result<Option<TicketDetail>, String> {
        let Some((repo, number)) = parse_external_id(key) else {
            return Err(format!(
                "unrecognised GitHub ticket key {key}; use owner/repo#number"
            ));
        };
        let Some(stdout) =
            self.run_gh_optional(&["api", &format!("repos/{repo}/issues/{number}")])?
        else {
            return Ok(None);
        };
        let issue: RestIssue = serde_json::from_slice(&stdout)
            .map_err(|err| format!("unexpected gh api output: {err}"))?;
        let comments_stdout = self.run_gh(&[
            "api",
            "--method",
            "GET",
            &format!("repos/{repo}/issues/{number}/comments"),
            "-f",
            "per_page=100",
        ])?;
        let comments: Vec<RestIssueComment> = serde_json::from_slice(&comments_stdout)
            .map_err(|err| format!("unexpected gh api output: {err}"))?;
        let merged = issue
            .pull_request
            .as_ref()
            .and_then(|pull_request| pull_request.get("merged_at"))
            .is_some_and(|value| !value.is_null());
        let status = if merged {
            "merged".to_string()
        } else {
            issue.state.clone()
        };
        let done = merged || issue.state.eq_ignore_ascii_case("closed");
        let assignee = issue
            .assignees
            .first()
            .or(issue.assignee.as_ref())
            .map(|user| user.login.clone());
        let ticket_key = format!("{repo}#{number}");
        let ticket = crate::api::schema::WorkItemTicketInfo {
            key: ticket_key.clone(),
            title: issue.title.clone(),
            status,
            done,
            assignee: assignee.clone(),
            updated_at: issue.updated_at.clone(),
            url: issue.html_url.clone(),
        };
        let source_item = SourceItem {
            external_id: format!("{}{ticket_key}", Event::Assigned.id_prefix()),
            title: issue.title,
            context: format!("#{number} {repo}"),
            author: assignee,
            url: issue.html_url,
            updated_at: issue.updated_at,
            tracker_state: None,
        };
        Ok(Some(TicketDetail {
            ticket,
            description: issue.body.unwrap_or_default(),
            comments: comments
                .into_iter()
                .map(|comment| crate::api::schema::WorkItemTicketComment {
                    author: comment
                        .user
                        .map_or_else(|| "unknown".into(), |user| user.login),
                    body: comment.body,
                })
                .collect(),
            source_item,
        }))
    }

    fn pick_next_plan(
        &self,
        context: &str,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        // The agent you work issues with; the first mapped repo picks the block.
        let first_repo = self
            .config
            .repos
            .first()
            .map_or("", |repo| repo.name.as_str());
        let config = self.branch_workflow(Event::Assigned, first_repo);
        if config.agent.is_empty() {
            return Err(
                "No agent configured for GitHub; set work_items.github.<block>.agent".into(),
            );
        }
        let directory =
            crate::worktree::default_checkout_path(worktree_directory, "pick-next", "github");
        Ok(ProvisionPlan {
            source: WorkspaceSource::Scratch(directory),
            workspace_label: format!("Pick next \u{b7} {}", self.label()),
            agent_name_hint: "pick-next-github".into(),
            brief: pick_next_brief(&self.config.repos, context),
            layout: WorkspaceLayout {
                agent: config.agent.clone(),
                agent_args: config.agent_args.clone(),
                tabs: config.tabs.clone(),
                diff_command: String::new(),
            },
            delete_branch: false,
        })
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

/// Approval, merge state and, when reviewers left any, how many comments they left.
/// `pr_author` is left out of the count: replies you posted are not feedback.
fn merge_summary(detail: &GithubDetail, pr_author: Option<&str>) -> String {
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
    let by_reviewer = |login: &str| Some(login) != pr_author;
    let comments = detail
        .latest_reviews
        .iter()
        .filter(|review| !review.body.trim().is_empty())
        .filter(|review| {
            review
                .author
                .as_ref()
                .is_none_or(|author| by_reviewer(&author.login))
        })
        .count()
        + detail
            .inline_comments
            .iter()
            .filter(|comment| by_reviewer(&comment.author))
            .count();
    match comments {
        0 => format!("{approved} · {state}"),
        1 => format!("{approved} · {state} · 1 comment"),
        n => format!("{approved} · {state} · {n} comments"),
    }
}

/// Merge choices: your default method first, then `work` on the review (the agent's
/// follow-ups before working on it by hand), then the other merge methods. With every method
/// blocked there is no default.
fn merge_choices(detail: Option<&GithubDetail>, mut work: Vec<WorkItemChoiceInfo>) -> ItemChoices {
    let Some(detail) = detail else {
        return ItemChoices {
            choices: work,
            default_choice_id: None,
        };
    };
    let blocker = merge_blocker(detail);
    let base = &detail.base_ref_name;
    let number = detail.number;
    let mut methods: Vec<WorkItemChoiceInfo> = detail
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
            options: Vec::new(),
        })
        .collect();
    let default = methods
        .iter()
        .find(|choice| choice.disabled_reason.is_none())
        .map(|choice| choice.choice_id.clone());
    let other_methods = methods.split_off(methods.len().min(1));
    // The modes list working on it by hand first; once it is approved, handing the
    // comments to the agent is the likelier next step.
    work.reverse();
    let mut choices = methods;
    choices.append(&mut work);
    choices.extend(other_methods);
    ItemChoices {
        choices,
        default_choice_id: default,
    }
}

/// Whether `mode` starts work on an issue whose pull request is in flight. That work is under
/// review, and a new issue branch would be a second attempt at it.
fn superseded_by_pull_request(item: &WorkItem, mode: ReviewMode) -> bool {
    matches!(mode, ReviewMode::StartIssue | ReviewMode::StartIssueAgent)
        && item.open_pull_request().is_some()
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
                    waiting: false,
                    detail: None,
                    summary: None,
                    error: Some(error),
                    done: false,
                }
            }
        };
        let merged = issue
            .pull_request
            .as_ref()
            .and_then(|pull_request| pull_request.get("merged_at"))
            .is_some_and(|value| !value.is_null());
        let done = merged || issue.state.eq_ignore_ascii_case("closed");
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
        // Unknown when gh will not say who you are: then no reminder, rather than a wrong one.
        let assigned_to_me = if issue.pull_request.is_some() {
            None
        } else {
            self.viewer_login().ok().map(|me| {
                issue
                    .assignees
                    .iter()
                    .chain(issue.assignee.as_ref())
                    .any(|user| user.login.eq_ignore_ascii_case(&me))
            })
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
            assigned_to_me,
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
            waiting: false,
            detail: serde_json::to_value(&detail).ok(),
            summary: Some(summary),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            done,
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
                id: comment.id,
                in_reply_to: comment.in_reply_to_id,
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
    fn branch_pull_request_state_reads_as_one_line() {
        let parse = |json: &str| {
            parse_branch_pull_request(json.as_bytes(), "github", "o/r", None)
                .expect("parses")
                .map(|pull| (pull.is_draft, pull.status))
        };
        assert_eq!(parse("[]"), None);
        assert_eq!(
            parse(
                r#"[{"number":7,"url":"u","state":"OPEN","isDraft":true,"reviewDecision":"",
                "statusCheckRollup":[{"status":"IN_PROGRESS","conclusion":""}]}]"#
            ),
            Some((true, "draft · checks running".into()))
        );
        assert_eq!(
            parse(
                r#"[{"number":7,"url":"u","state":"OPEN","isDraft":false,
                "reviewDecision":"CHANGES_REQUESTED",
                "statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"},
                {"state":"FAILURE"}]}]"#
            ),
            Some((false, "changes requested · CI failing".into()))
        );
        // A merged draft is no longer waiting on you.
        assert_eq!(
            parse(r#"[{"number":7,"url":"u","state":"MERGED","isDraft":true}]"#),
            Some((false, "merged".into()))
        );
        // Without required reviews GitHub reports no decision: the latest reviews decide.
        assert_eq!(
            parse(
                r#"[{"number":7,"url":"u","state":"OPEN","isDraft":false,"reviewDecision":"",
                "latestReviews":[{"state":"APPROVED"}]}]"#
            ),
            Some((false, "approved".into()))
        );
        assert_eq!(
            parse(
                r#"[{"number":7,"url":"u","state":"OPEN","isDraft":false,"reviewDecision":"",
                "latestReviews":[{"state":"APPROVED"},{"state":"CHANGES_REQUESTED"}]}]"#
            ),
            Some((false, "changes requested".into()))
        );
        assert_eq!(
            parse(r#"[{"number":7,"url":"u","state":"OPEN","isDraft":false}]"#),
            Some((false, "awaiting review".into()))
        );
    }

    #[test]
    fn batched_pull_request_statuses_line_up_with_the_asked_pulls() {
        let pulls = vec![
            ("o/r".to_string(), 1),
            ("o/r".to_string(), 2),
            ("gone/repo".to_string(), 3),
        ];
        let (query, args) = pull_request_status_query(&pulls);
        assert!(
            query.contains("p2: repository(owner: $o2, name: $n2)"),
            "{query}"
        );
        assert_eq!(&args[..6], ["-f", "o0=o", "-f", "n0=r", "-F", "p0=1"]);
        // GitHub answers what it resolved and nulls the rest.
        let json = br#"{"data":{
            "p0":{"pullRequest":{"number":1,"url":"u1","state":"OPEN","isDraft":false,
                "reviewDecision":null,
                "latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},
                "reviewRequests":{"nodes":[{"requestedReviewer":{}},
                    {"requestedReviewer":{"login":"Me"}}]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}}},
            "p1":{"pullRequest":{"number":2,"url":"u2","state":"OPEN","isDraft":true,
                "reviewDecision":"REVIEW_REQUIRED","latestReviews":{"nodes":[]},
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]}}},
            "p2":null},
            "errors":[{"type":"NOT_FOUND","path":["p2"]}]}"#;
        let statuses =
            parse_pull_request_statuses(json, "github", &pulls, Some("me")).expect("parses");
        let lines: Vec<Option<(bool, &str)>> = statuses
            .iter()
            .map(|status| {
                status
                    .as_ref()
                    .map(|pull| (pull.is_draft, pull.status.as_str()))
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                // Someone else's approval does not stand in for the review asked of you.
                Some((
                    false,
                    "awaiting your review · approved by @alice · CI failing"
                )),
                Some((true, "draft · checks running")),
                None,
            ]
        );
    }

    #[test]
    fn review_status_reads_from_your_side_and_names_the_other_reviewers() {
        let review = |login: &str, state: &str| GithubReview {
            author: Some(GithubLogin {
                login: login.into(),
            }),
            state: state.into(),
            body: String::new(),
        };
        let requested = |login: &str| GithubReviewRequest {
            login: login.into(),
        };
        let me = Some("me");
        let status =
            |decision: Option<&str>, reviews: &[GithubReview], requests: &[GithubReviewRequest]| {
                review_status(decision, reviews, requests, me)
            };

        assert_eq!(
            status(Some("APPROVED"), &[review("me", "APPROVED")], &[]),
            "you approved"
        );
        // Your approval is not enough when the repository wants more.
        assert_eq!(
            status(Some("REVIEW_REQUIRED"), &[review("me", "APPROVED")], &[]),
            "you approved · awaiting review"
        );
        // Asked again after requesting changes: the request is what is left for you.
        assert_eq!(
            status(
                Some("CHANGES_REQUESTED"),
                &[review("me", "CHANGES_REQUESTED")],
                &[requested("me")]
            ),
            "awaiting your review"
        );
        assert_eq!(
            status(
                None,
                &[review("me", "APPROVED"), review("bob", "CHANGES_REQUESTED")],
                &[]
            ),
            "you approved · changes requested by @bob"
        );
        // Your own pull request: only the others' verdict, with who gave it.
        assert_eq!(
            status(
                Some("APPROVED"),
                &[
                    review("ann", "APPROVED"),
                    review("bob", "APPROVED"),
                    review("cyd", "COMMENTED"),
                    review("dee", "APPROVED"),
                ],
                &[]
            ),
            "approved by @ann, @bob +1"
        );
        assert_eq!(
            status(Some("REVIEW_REQUIRED"), &[], &[requested("bob")]),
            "awaiting review"
        );
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
                    tracker_state: None,
                },
                SourceItem {
                    external_id: "x/y#3".into(),
                    title: "WIP".into(),
                    // Its state, draft included, shows on its own status line.
                    context: "#3 x/y".into(),
                    author: Some("bob".into()),
                    url: "https://github.com/x/y/pull/3".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    tracker_state: None,
                },
            ]
        );
        // A mention has no status line, so its context still says draft.
        let mentions = parse_search(json, Event::Mentioned).expect("parses");
        assert_eq!(mentions[1].context, "#3 mention · x/y · draft");
    }

    #[test]
    fn search_result_without_repository_is_skipped() {
        let json = br#"{"items":[{"number":1,"title":"t","html_url":"u","updated_at":"x"}]}"#;
        assert_eq!(
            parse_search(json, Event::ReviewRequested).expect("parses"),
            Vec::new()
        );
    }

    #[test]
    fn ticket_search_reports_status_done_and_assignee() {
        let json = br#"{"items":[
            {"number":12,"title":"Fix it","html_url":"https://github.com/o/r/pull/12",
             "updated_at":"2026-01-02T00:00:00Z","state":"open",
             "repository_url":"https://api.github.com/repos/o/r",
             "assignees":[{"login":"alice"}]},
            {"number":3,"title":"Ship it","html_url":"https://github.com/o/r/pull/3",
             "updated_at":"2026-01-01T00:00:00Z","state":"closed",
             "pull_request":{"merged_at":"2026-01-01T00:00:00Z"},
             "repository_url":"https://api.github.com/repos/o/r"}]}"#;
        let tickets = parse_ticket_search(json).expect("parses");
        assert_eq!(
            tickets,
            vec![
                crate::api::schema::WorkItemTicketInfo {
                    key: "o/r#12".into(),
                    title: "Fix it".into(),
                    status: "open".into(),
                    done: false,
                    assignee: Some("alice".into()),
                    updated_at: "2026-01-02T00:00:00Z".into(),
                    url: "https://github.com/o/r/pull/12".into(),
                },
                crate::api::schema::WorkItemTicketInfo {
                    key: "o/r#3".into(),
                    title: "Ship it".into(),
                    status: "merged".into(),
                    done: true,
                    assignee: None,
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    url: "https://github.com/o/r/pull/3".into(),
                },
            ]
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
            review_requests: Vec::new(),
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
            tracker_state: None,
            linked_pull_request: None,
            own_pull_request: None,
            linked_ticket: None,
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
            action_running: None,
            action_outcome: None,
            action_error: None,
            waiting: false,
            manual: false,
            is_pick_next: false,
            start_reminder_muted: false,
            phase_before_action: None,
            brief_failure_outdated: false,
            brief_failed_at: None,
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
    fn review_requests_offer_one_review_choice_with_a_worktree_and_a_post_switch() {
        let source = mapped_source(repo_config());
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let choices = source.choices(&item);
        let ids: Vec<&str> = choices
            .choices
            .iter()
            .map(|choice| choice.choice_id.as_str())
            .collect();
        assert_eq!(ids, ["review"]);

        let review = &choices.choices[0];
        assert_eq!(review.label, "Review");
        assert_eq!(review.action, WorkItemChoiceAction::ProvisionWorkspace);
        assert_eq!(review.disabled_reason, None);
        // A worktree unless switched off, as the choice this replaces always made one; a
        // comment on GitHub is visible to everyone, so only when asked for.
        let switches: Vec<(&str, &str, bool)> = review
            .options
            .iter()
            .map(|option| {
                (
                    option.option_id.as_str(),
                    option.label.as_str(),
                    option.default,
                )
            })
            .collect();
        assert_eq!(
            switches,
            [
                ("worktree", "Create worktree", true),
                ("post", "Post to GitHub", false)
            ]
        );
    }

    #[test]
    fn an_unmapped_review_request_defaults_to_review_rather_than_linking() {
        // Linking is listed first, but a review of the diff can start straight away.
        let source = GithubSource::new(GithubWorkItemsConfig {
            clone_root: "~/projects".into(),
            ..GithubWorkItemsConfig::default()
        });
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 500, 0)])));
        assert_eq!(
            source.choices(&item).default_choice_id.as_deref(),
            Some("review")
        );
    }

    #[test]
    fn an_unmapped_repository_offers_no_worktree_switch() {
        // Without a local clone there is nothing to check out, so only posting is a switch.
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let choices = source.choices(&work_item("o/r", Some(&detail(&[("src/a.rs", 500, 0)]))));
        let review = choices
            .choices
            .iter()
            .find(|choice| choice.choice_id == "review")
            .expect("review offered");
        let switches: Vec<&str> = review
            .options
            .iter()
            .map(|option| option.option_id.as_str())
            .collect();
        assert_eq!(switches, ["post"]);
    }

    #[test]
    fn clone_root_offers_linking_an_unmapped_repository_first() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            clone_root: "~/projects".into(),
            ..GithubWorkItemsConfig::default()
        });
        let choices = source.choices(&work_item("o/r", Some(&detail(&[("src/a.rs", 500, 0)]))));
        let link = &choices.choices[0];
        assert_eq!(link.choice_id, LINK_CLONE_CHOICE_ID);
        assert_eq!(link.action, WorkItemChoiceAction::Perform);
        assert_eq!(link.disabled_reason, None);
    }

    #[test]
    fn mapped_repository_offers_no_linking() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            clone_root: "~/projects".into(),
            repos: vec![repo_config()],
            ..GithubWorkItemsConfig::default()
        });
        let choices = source.choices(&work_item("O/R", Some(&detail(&[("src/a.rs", 500, 0)]))));
        assert!(choices
            .choices
            .iter()
            .all(|choice| choice.choice_id != LINK_CLONE_CHOICE_ID));
    }

    #[test]
    fn remote_urls_name_their_repository_whatever_the_protocol() {
        for url in [
            "git@github.com:o/r.git",
            "https://github.com/O/R",
            "https://github.com/o/r.git/",
            "ssh://git@ghe.example.com/o/r.git",
        ] {
            assert!(remote_url_names(url, "o/r"), "{url}");
        }
        for url in ["git@github.com:o/rr.git", "https://github.com/me/r", "r"] {
            assert!(!remote_url_names(url, "o/r"), "{url}");
        }
    }

    #[test]
    fn clone_directory_names_reject_path_tricks() {
        assert!(is_path_segment(".github"));
        assert!(is_path_segment("my-repo_2.0"));
        for segment in ["", ".", "..", "a/b", "a\\b", "~"] {
            assert!(!is_path_segment(segment), "{segment:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn linking_finds_the_main_clone_whose_remote_serves_the_repository() {
        let root = std::env::temp_dir().join(format!("herdr-link-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let git = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git runs")
                .status;
            assert!(status.success(), "git {args:?}");
        };
        // Named after the repository, but a clone of another one.
        let other = root.join("r");
        // A fork: origin is yours, upstream serves the repository.
        let fork = root.join("my-fork");
        for dir in [&other, &fork] {
            std::fs::create_dir_all(dir).expect("creates");
            git(dir, &["init", "--quiet"]);
        }
        git(
            &other,
            &["remote", "add", "origin", "git@github.com:x/r.git"],
        );
        git(
            &fork,
            &["remote", "add", "origin", "git@github.com:me/r.git"],
        );
        git(
            &fork,
            &["remote", "add", "upstream", "https://github.com/o/r.git"],
        );
        // A worktree of the fork: its `.git` is a file.
        let worktree = root.join("a-worktree");
        std::fs::create_dir_all(&worktree).expect("creates");
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").expect("writes");
        let source = GithubSource::new(GithubWorkItemsConfig {
            clone_root: root.display().to_string(),
            ..GithubWorkItemsConfig::default()
        });
        let linked = source.link_clone(&work_item("o/r", None));
        let _ = std::fs::remove_dir_all(&root);
        let (clone, message) = linked.expect("links");
        assert_eq!(
            clone,
            LinkedClone {
                source_id: "github".into(),
                name: "o/r".into(),
                path: fork.clone(),
                remote: "upstream".into(),
            }
        );
        assert_eq!(message, format!("Linked o/r to {}", fork.display()));
    }

    #[test]
    fn reviewing_in_a_worktree_plans_a_review_branch_from_the_fetched_pull_request() {
        let source = mapped_source(repo_config());
        let change = detail(&[("src/a.rs", 40, 2), ("src/b.rs", 1, 1)]);
        let plan = source
            .provision_plan(
                &work_item("o/r", Some(&change)),
                "review",
                &options(&["worktree"]),
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
            plan.layout.tabs[1].command,
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
                    tabs: vec![crate::config::WorkspaceTabConfig {
                        label: "editor".into(),
                        command: "hx .".into(),
                        fallback: String::new(),
                    }],
                    agent_args: vec!["--model".into(), "opus".into()],
                    ..ReviewRequestedConfig::default()
                },
            ],
            ..GithubWorkItemsConfig::default()
        });
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let plan = source
            .provision_plan(
                &item,
                "review",
                &options(&["worktree"]),
                Path::new("/worktrees"),
            )
            .expect("plan");
        assert!(!plan.delete_branch);
        assert_eq!(plan.layout.tabs[0].command, "hx .");
        assert_eq!(plan.layout.agent, "claude");
        assert_eq!(plan.layout.agent_args, ["--model", "opus"]);
        assert!(source.remove_on_resolved(&item));
        assert!(!source.remove_on_resolved(&work_item("x/y", None)));
    }

    fn options(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn reviewing_without_a_worktree_downloads_the_diff_with_gh() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let change = detail(&[("src/a.rs", 40, 2)]);
        let item = work_item("o/r", Some(&change));
        let choices = source.choices(&item);
        assert_eq!(choices.choices[0].choice_id, "review");
        assert_eq!(
            choices.choices[0].disabled_reason, None,
            "the agent needs no clone to review a diff"
        );
        let plan = source
            .provision_plan(&item, "review", &[], Path::new("/worktrees"))
            .expect("plan");
        let WorkspaceSource::Download(download) = &plan.source else {
            panic!("a review without a worktree downloads the diff");
        };
        assert_eq!(download.directory, Path::new("/worktrees/r/pr-5-agent"));
        assert_eq!(download.program, "gh");
        assert_eq!(download.args[..5], ["pr", "diff", "5", "--repo", "o/r"]);
        assert_eq!(download.file_name, "pr-5.diff");
        assert!(plan.brief.contains("./pr-5.diff"));
    }

    #[test]
    fn the_review_switches_make_four_ways_to_review() {
        let source = mapped_source(repo_config());
        let item = work_item("o/r", Some(&detail(&[("src/a.rs", 40, 2)])));
        let plan = |on: &[&str]| {
            source
                .provision_plan(&item, "review", &options(on), Path::new("/worktrees"))
                .expect("plan")
        };
        let post = "gh pr review 5 --repo o/r --comment --body-file";
        let worktree = "This directory is a worktree checked out at the pull request head";

        // Neither: a downloaded diff, the findings reported back, nothing posted.
        let report = plan(&[]);
        assert!(matches!(report.source, WorkspaceSource::Download(_)));
        assert!(report.brief.contains("./pr-5.diff"));
        assert!(report.brief.contains("Start reviewing now"));
        assert!(report.brief.contains("Do not post anything to GitHub"));
        assert!(!report.brief.contains(post));

        // Posting alone: the same diff, and one review that only comments.
        let comment = plan(&["post"]);
        assert!(matches!(comment.source, WorkspaceSource::Download(_)));
        assert!(comment.brief.contains("./pr-5.diff"));
        assert!(comment.brief.contains(post));
        assert!(comment.brief.contains("never approve or request changes"));
        assert!(!comment.brief.contains("Do not post anything to GitHub"));

        // A worktree alone: the agent reviews in it, and nothing changes or is posted.
        let local = plan(&["worktree"]);
        assert!(matches!(local.source, WorkspaceSource::Worktree(_)));
        assert!(local.brief.contains(worktree));
        assert!(local.brief.contains("Start reviewing now"));
        assert!(local
            .brief
            .contains("Do not modify files, commit, or post anything to GitHub"));
        assert!(!local.brief.contains(post));

        // Both: reviewed in the worktree, then posted as a comment.
        let both = plan(&["worktree", "post"]);
        assert!(matches!(both.source, WorkspaceSource::Worktree(_)));
        assert!(both.brief.contains(worktree));
        assert!(both.brief.contains("Start reviewing now"));
        assert!(both.brief.contains(post));
        assert!(both.brief.contains("never approve or request changes"));
        assert!(both.brief.contains("Do not modify files or commit"));
        assert!(both.brief.contains("outside this worktree"));
        assert!(!both.brief.contains("./pr-5.diff"));
        assert!(!both.brief.contains("Do not post anything to GitHub"));
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
        // The agent always reviews, so there is no review without one.
        assert_eq!(disabled, vec!["review"]);
        assert!(source
            .provision_plan(&item, "review", &[], Path::new("/worktrees"))
            .is_err());
    }

    #[test]
    fn a_review_that_cannot_run_leaves_no_default() {
        let without_agent = GithubSource::new(GithubWorkItemsConfig {
            repos: vec![repo_config()],
            review_requested: vec![ReviewRequestedConfig {
                agent: String::new(),
                ..ReviewRequestedConfig::default()
            }],
            ..GithubWorkItemsConfig::default()
        });
        let large = detail(&[("src/a.rs", 40, 2)]);
        // Reviewing is the default while it can run, and nothing is once it cannot.
        assert_eq!(
            default_for(&mapped_source(repo_config()), &large).as_deref(),
            Some("review")
        );
        assert_eq!(default_for(&without_agent, &large), None);
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
                id: 0,
                in_reply_to: None,
                path: "src/a.rs".into(),
                line: Some(12),
                author: "bob".into(),
                body: "This can panic.".into(),
            }],
            ..detail(&[("src/a.rs", 40, 2)])
        }
    }

    fn review(login: &str, state: &str) -> GithubReview {
        GithubReview {
            author: Some(GithubLogin {
                login: login.into(),
            }),
            state: state.into(),
            body: String::new(),
        }
    }

    fn requested(logins: &[&str]) -> Vec<GithubReviewRequest> {
        logins
            .iter()
            .map(|login| GithubReviewRequest {
                login: (*login).into(),
            })
            .collect()
    }

    #[test]
    fn feedback_tags_each_inline_comment_with_its_thread() {
        let detail = GithubDetail {
            reviews: Vec::new(),
            inline_comments: vec![
                GithubInlineComment {
                    id: 101,
                    in_reply_to: Some(100),
                    path: "src/a.rs".into(),
                    line: Some(3),
                    author: "bob".into(),
                    body: "Still panics.".into(),
                },
                GithubInlineComment {
                    id: 0,
                    in_reply_to: None,
                    path: "src/b.rs".into(),
                    line: None,
                    author: "bob".into(),
                    body: "Stored before ids.".into(),
                },
            ],
            ..detail(&[])
        };
        assert_eq!(
            feedback_list(&detail, Event::ChangesRequested),
            "- [thread 100] src/a.rs:3 @bob: Still panics.\n- src/b.rs @bob: Stored before ids."
        );
    }

    #[test]
    fn a_comment_after_requesting_changes_keeps_the_request() {
        let detail = GithubDetail {
            reviews: vec![
                review("bob", "CHANGES_REQUESTED"),
                review("bob", "COMMENTED"),
            ],
            ..detail(&[])
        };
        assert_eq!(changes_requesters(&detail), vec!["bob".to_string()]);
    }

    #[test]
    fn waiting_for_rereview_once_every_requester_is_asked_again() {
        let detail = GithubDetail {
            reviews: vec![
                review("bob", "CHANGES_REQUESTED"),
                review("carol", "CHANGES_REQUESTED"),
            ],
            review_requests: requested(&["carol", "", "bob"]),
            ..detail(&[])
        };
        assert_eq!(
            waiting_for_rereview(&detail),
            Some(vec!["bob".to_string(), "carol".to_string()])
        );
    }

    #[test]
    fn not_waiting_while_a_requester_is_not_asked_again() {
        let detail = GithubDetail {
            reviews: vec![
                review("bob", "CHANGES_REQUESTED"),
                review("carol", "CHANGES_REQUESTED"),
            ],
            review_requests: requested(&["bob"]),
            ..detail(&[])
        };
        assert_eq!(waiting_for_rereview(&detail), None);
    }

    #[test]
    fn not_waiting_once_the_requester_approved() {
        let detail = GithubDetail {
            reviews: vec![
                review("bob", "CHANGES_REQUESTED"),
                review("bob", "APPROVED"),
            ],
            review_requests: requested(&["bob"]),
            ..detail(&[])
        };
        assert_eq!(waiting_for_rereview(&detail), None);
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
        assert_eq!(ids, vec!["address", "address_agent", "push_reply"]);
        assert_eq!(choices.default_choice_id.as_deref(), Some("address"));
        assert!(source
            .provision_plan(
                &changes_item(Some(&feedback_detail())),
                "review",
                &[],
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
                &[],
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
            .contains("commit them on this branch. Do not push, reply or comment on GitHub"));
    }

    fn push_reply_choice(item: &WorkItem) -> WorkItemChoiceInfo {
        mapped_source(repo_config())
            .choices(item)
            .choices
            .into_iter()
            .find(|choice| choice.choice_id == "push_reply")
            .expect("offered")
    }

    #[test]
    fn pushing_and_replying_needs_a_local_workspace() {
        let item = changes_item(Some(&feedback_detail()));
        assert_eq!(
            push_reply_choice(&item).disabled_reason.as_deref(),
            Some("Work on it locally first")
        );
        let local = WorkItem {
            workspace_id: Some("w1".into()),
            ..item
        };
        let choice = push_reply_choice(&local);
        assert_eq!(choice.disabled_reason, None);
        assert_eq!(choice.action, WorkItemChoiceAction::BriefAgent);
    }

    #[test]
    fn push_and_reply_brief_re_requests_review_from_each_requester() {
        let detail = GithubDetail {
            reviews: vec![
                review("bob", "CHANGES_REQUESTED"),
                review("carol", "CHANGES_REQUESTED"),
            ],
            ..feedback_detail()
        };
        let brief = mapped_source(repo_config())
            .follow_up_brief(&changes_item(Some(&detail)), "push_reply")
            .expect("brief");
        assert!(brief.contains(
            "4. Re-request review from @bob, @carol: `gh api --method POST \
             repos/o/r/pulls/5/requested_reviewers -f 'reviewers[]=bob' -f 'reviewers[]=carol'`"
        ));
    }

    #[test]
    fn push_and_reply_brief_does_not_re_request_without_requesters() {
        let detail = GithubDetail {
            reviews: vec![review("bob", "APPROVED")],
            ..feedback_detail()
        };
        let brief = mapped_source(repo_config())
            .follow_up_brief(&changes_item(Some(&detail)), "push_reply")
            .expect("brief");
        assert!(
            brief.contains("4. Nobody currently requests changes, so do not re-request review.")
        );
    }

    #[test]
    fn fork_pull_request_branch_starts_from_the_pull_ref() {
        let source = mapped_source(repo_config());
        let fork = GithubDetail {
            is_cross_repository: true,
            ..feedback_detail()
        };
        let plan = source
            .provision_plan(&changes_item(Some(&fork)), "address", &[], Path::new("/w"))
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
                    tracker_state: None,
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
            assigned_to_me: None,
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
        let plan = source
            .provision_plan(&item, "fix_checks_agent", &[], Path::new("/w"))
            .expect("plan");
        let WorkspaceSource::Worktree(spec) = &plan.source else {
            panic!("worktree");
        };
        assert_eq!(spec.branch, "feature");
        assert!(spec.reuse_branch);
        assert!(plan
            .brief
            .contains("- CI / rspec: https://github.com/o/r/actions/runs/42/job/7"));
        assert!(plan
            .brief
            .contains("commit the fix on this branch. Do not push"));
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
            .provision_plan(&item, "start_issue", &[], Path::new("/w"))
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
            .provision_plan(&unknown_base, "start_issue", &[], Path::new("/w"))
            .is_err());
    }

    #[test]
    fn an_assigned_issue_whose_pull_request_is_open_stops_offering_to_start_it_again() {
        let source = mapped_source(repo_config());
        let with_pull_request = |status: &str| WorkItem {
            linked_pull_request: Some(crate::api::schema::WorkItemPullRequestInfo {
                source_id: "github".into(),
                repo: "o/r".into(),
                number: 9,
                url: "https://github.com/o/r/pull/9".into(),
                is_draft: status == "draft",
                status: status.into(),
            }),
            workspace_id: Some("w1".into()),
            ..event_item(
                ASSIGNED_PREFIX,
                serde_json::to_value(issue_detail("main")).unwrap(),
            )
        };
        let offered = |item: &WorkItem| {
            let choices = source.choices(item);
            let ids: Vec<String> = choices
                .choices
                .into_iter()
                .map(|choice| choice.choice_id)
                .collect();
            (ids, choices.default_choice_id)
        };

        // A new issue branch would be a second attempt at work that is under review.
        let (ids, default) = offered(&with_pull_request("approved"));
        assert!(ids.is_empty(), "{ids:?}");
        assert_eq!(default, None);

        // Once it is merged, the issue can be worked on again.
        let (ids, default) = offered(&with_pull_request("merged"));
        assert_eq!(ids, ["start_issue", "start_issue_agent"]);
        assert_eq!(default.as_deref(), Some("start_issue"));
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
        assert_eq!(choices.default_choice_id, None);
        let plan = source
            .provision_plan(&item, "thread_agent", &[], Path::new("/w"))
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

    /// A `gh` stand-in for `api repos/o/r/issues/N` and its `/comments`, one fixed issue.
    #[cfg(unix)]
    fn fake_gh_issue(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-fake-gh-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *"repos/o/r/issues/5/comments"*)
    printf '[{"body":"first","user":{"login":"carol"}}]'
    ;;
  *"repos/o/r/issues/5"*)
    printf '{"number":5,"title":"Ship it","body":"Do the thing","state":"closed",
             "updated_at":"2026-01-01T00:00:00Z","html_url":"https://github.com/o/r/issues/5",
             "assignees":[{"login":"alice"}]}'
    ;;
  *"repos/o/r/issues/404"*)
    echo "gh: Not Found (HTTP 404)" >&2
    exit 1
    ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn fetch_returns_the_ticket_with_its_description_and_comments() {
        let gh = fake_gh_issue("fetch-found");
        let source = GithubSource::new(GithubWorkItemsConfig {
            gh_path: gh.display().to_string(),
            ..GithubWorkItemsConfig::default()
        });
        let detail = source.fetch("o/r#5").expect("fetch").expect("found");
        let _ = std::fs::remove_dir_all(gh.parent().unwrap());
        assert_eq!(detail.ticket.key, "o/r#5");
        assert_eq!(detail.ticket.title, "Ship it");
        assert_eq!(detail.ticket.status, "closed");
        assert!(detail.ticket.done);
        assert_eq!(detail.ticket.assignee.as_deref(), Some("alice"));
        assert_eq!(detail.description, "Do the thing");
        assert_eq!(detail.comments.len(), 1);
        assert_eq!(detail.comments[0].author, "carol");
        assert_eq!(detail.source_item.external_id, "assigned:o/r#5");
    }

    #[cfg(unix)]
    #[test]
    fn fetch_returns_none_for_a_missing_ticket() {
        let gh = fake_gh_issue("fetch-missing");
        let source = GithubSource::new(GithubWorkItemsConfig {
            gh_path: gh.display().to_string(),
            ..GithubWorkItemsConfig::default()
        });
        let result = source.fetch("o/r#404").expect("fetch");
        let _ = std::fs::remove_dir_all(gh.parent().unwrap());
        assert!(result.is_none());
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
        // Your default method, then the review follow-ups, agent first, then the others.
        assert_eq!(
            ids,
            vec![
                "merge_squash",
                "push_reply",
                "address_agent",
                "address",
                "merge_commit"
            ]
        );
        assert_eq!(choices.default_choice_id.as_deref(), Some("merge_squash"));
        let squash = &choices.choices[0];
        assert_eq!(squash.action, WorkItemChoiceAction::Perform);
        assert!(squash
            .confirm
            .as_deref()
            .is_some_and(|prompt| prompt.contains("cannot be undone")));
        assert_eq!(
            merge_summary(&merge_detail("CLEAN"), Some("me")),
            "Approved by tony · ready to merge"
        );
    }

    #[test]
    fn merge_summary_counts_reviewer_comments_but_not_your_replies() {
        let comment = |id: u64, author: &str| GithubInlineComment {
            id,
            in_reply_to: (id > 1).then_some(1),
            path: "src/a.rs".into(),
            line: Some(3),
            author: author.into(),
            body: "nit: rename this".into(),
        };
        let mut detail = merge_detail("CLEAN");
        detail.latest_reviews[0].body = "LGTM, one nit".into();
        detail.inline_comments = vec![comment(1, "tony"), comment(2, "me")];
        assert_eq!(
            merge_summary(&detail, Some("me")),
            "Approved by tony · ready to merge · 2 comments"
        );
        // The brief carries the approval's text alongside the inline thread.
        let feedback = feedback_list(&detail, Event::ReadyToMerge);
        assert!(
            feedback.contains("@tony (approved): LGTM, one nit"),
            "{feedback}"
        );
        assert!(
            feedback.contains("[thread 1] src/a.rs:3 @tony"),
            "{feedback}"
        );
    }

    #[test]
    fn blocked_merge_is_disabled_with_the_reason_and_leaves_no_default() {
        let source = GithubSource::new(GithubWorkItemsConfig::default());
        let choices = source.choices(&merge_item(&merge_detail("BEHIND")));
        assert_eq!(
            choices.choices[0].disabled_reason.as_deref(),
            Some("Behind main; update the branch first")
        );
        assert_eq!(choices.default_choice_id, None);
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
