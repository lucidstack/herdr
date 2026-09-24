//! GitHub pull request review requests, read through the GitHub CLI.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};
use crate::config::{
    GithubRepoConfig, GithubWorkItemsConfig, OnResolvedConfig, ReviewRequestedConfig,
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
const GITHUB_CHOICE_ID: &str = "github";
const MAX_WORKSPACE_LABEL_CHARS: usize = 40;
const MAX_BRIEF_BODY_CHARS: usize = 4000;
const MAX_BRIEF_FILES: usize = 100;

pub(crate) struct GithubSource {
    config: GithubWorkItemsConfig,
    /// Review-request blocks with compiled patterns, in configuration order.
    review_requested: Vec<Workflow>,
    /// Defaults used when no block matches a repository.
    fallback: Workflow,
    build_error: Option<String>,
}

/// One `[[work_items.github.review_requested]]` block, ready for matching.
struct Workflow {
    config: ReviewRequestedConfig,
    docs: Vec<Regex>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubFile {
    pub path: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

/// `owner/repo#123` split into repository and number.
fn parse_external_id(external_id: &str) -> Option<(&str, u64)> {
    let (repo, number) = external_id.rsplit_once('#')?;
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
    fn mode_unavailable(&self, mode: ReviewMode, repo: &str, mapped: bool) -> Option<String> {
        if mode.checks_out() && !mapped {
            return Some(format!("No local checkout configured for {repo}"));
        }
        if mode.needs_agent() && self.workflow(repo).config.agent.is_empty() {
            return Some(format!("No agent configured for {repo} review requests"));
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

fn item_detail(item: &WorkItem) -> Option<GithubDetail> {
    item.detail
        .clone()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Heuristic default: small or documentation-only changes are quicker to review on GitHub.
fn default_choice(
    detail: Option<&GithubDetail>,
    mapped: bool,
    small_diff_lines: u64,
    docs: &[Regex],
) -> &'static str {
    if !mapped {
        return GITHUB_CHOICE_ID;
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

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// How a pull request is reviewed once the user picks a provisioning choice.
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
}

impl ReviewMode {
    const ALL: [Self; 4] = [
        Self::Local,
        Self::LocalAgentReview,
        Self::AgentReport,
        Self::AgentPost,
    ];

    fn choice_id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LocalAgentReview => "local_agent",
            Self::AgentReport => "agent_report",
            Self::AgentPost => "agent_post",
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
            Self::LocalAgentReview => "Review locally, agent reviews first",
            Self::AgentReport => "Agent review, report back to me",
            Self::AgentPost => "Agent review, post on GitHub",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Local => "Worktree and tools; the agent gets the context and waits",
            Self::LocalAgentReview => "Worktree and tools; the agent starts reviewing",
            Self::AgentReport => "No checkout; the agent reviews with gh and reports here",
            Self::AgentPost => "No checkout; the agent reviews and comments on the PR",
        }
    }

    fn checks_out(self) -> bool {
        matches!(self, Self::Local | Self::LocalAgentReview)
    }

    fn needs_agent(self) -> bool {
        self != Self::Local
    }
}

fn diff_file_name(number: u64) -> String {
    format!("pr-{number}.diff")
}

fn brief(repo: &str, item: &WorkItem, detail: &GithubDetail, mode: ReviewMode) -> String {
    let author = item.author.as_deref().unwrap_or("unknown");
    let head: String = detail.head_ref_oid.chars().take(8).collect();
    let body = if detail.body.trim().is_empty() {
        "(no description)".to_string()
    } else {
        truncate_chars(detail.body.trim(), MAX_BRIEF_BODY_CHARS)
    };
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
        files = files.join("\n"),
    )
}

fn gh_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("gh auth login") {
        return "GitHub CLI is not authenticated; run gh auth login".into();
    }
    format!("gh api failed: {}", failure_detail(output))
}

fn parse_search(bytes: &[u8]) -> Result<Vec<SourceItem>, String> {
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
            // two pull requests of the same repository apart.
            let mut context = format!("#{} {repo}", item.number);
            if item.draft == Some(true) {
                context.push_str(" · draft");
            }
            Some(SourceItem {
                external_id: format!("{repo}#{}", item.number),
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
        let query = format!("q={}", self.config.queries.review_requested);
        let stdout = self.run_gh(&[
            "api",
            "--method",
            "GET",
            "search/issues",
            "-f",
            &query,
            "-f",
            "per_page=50",
            "-f",
            "sort=updated",
            "-f",
            "order=desc",
        ])?;
        parse_search(&stdout)
    }

    fn prepare(&self, item: &SourceItem) -> PreparedItem {
        let Some((repo, number)) = parse_external_id(&item.external_id) else {
            return PreparedItem {
                detail: None,
                summary: None,
                error: Some(format!("unrecognised pull request id {}", item.external_id)),
            };
        };
        let number_arg = number.to_string();
        let viewed = self.run_gh(&[
            "pr",
            "view",
            &number_arg,
            "--repo",
            repo,
            "--json",
            "number,title,body,url,additions,deletions,changedFiles,files,baseRefName,headRefName,headRefOid,author",
        ]);
        let detail = match viewed.and_then(|stdout| {
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
        let summary = format!(
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
        let error = self
            .repo(repo)
            .and_then(|mapped| self.prefetch(mapped, number).err())
            .map(|detail| format!("prefetch failed: {detail}"));
        PreparedItem {
            detail: serde_json::to_value(&detail).ok(),
            summary: Some(summary),
            error,
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        let repo = parse_external_id(&item.external_id).map(|(repo, _)| repo);
        let mapped = repo.and_then(|repo| self.repo(repo)).is_some();
        let detail = item_detail(item);
        let mut choices: Vec<WorkItemChoiceInfo> = ReviewMode::ALL
            .into_iter()
            .map(|mode| WorkItemChoiceInfo {
                choice_id: mode.choice_id().into(),
                label: mode.label().into(),
                description: Some(mode.description().into()),
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: self.mode_unavailable(
                    mode,
                    repo.unwrap_or(&item.external_id),
                    mapped,
                ),
            })
            .collect();
        choices.push(WorkItemChoiceInfo {
            choice_id: GITHUB_CHOICE_ID.into(),
            label: "Review on GitHub".into(),
            description: Some("Open the pull request in the browser".into()),
            action: WorkItemChoiceAction::OpenUrl {
                url: item.url.clone(),
            },
            disabled_reason: None,
        });
        let workflow = self.workflow(repo.unwrap_or(&item.external_id));
        ItemChoices {
            choices,
            default_choice_id: Some(
                default_choice(
                    detail.as_ref(),
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
        let mode = ReviewMode::from_choice_id(choice_id)
            .ok_or_else(|| format!("choice {choice_id} does not provision a workspace"))?;
        let (repo_name, number) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised pull request id {}", item.external_id))?;
        let mapped = self.repo(repo_name);
        if let Some(reason) = self.mode_unavailable(mode, repo_name, mapped.is_some()) {
            return Err(reason);
        }
        let detail = item_detail(item).ok_or_else(|| {
            "pull request details are not available yet; try again shortly".to_string()
        })?;
        let workflow = &self.workflow(repo_name).config;
        let short_name = repo_name.rsplit('/').next().unwrap_or(repo_name);
        let workspace_label = truncate_chars(
            &format!("#{number} {}", item.title),
            MAX_WORKSPACE_LABEL_CHARS,
        );
        let agent_name_hint = format!("review-{number}");
        let brief = brief(repo_name, item, &detail, mode);
        let layout = WorkspaceLayout {
            agent: workflow.agent.clone(),
            editor_command: workflow.editor_command.clone(),
            lazygit_command: workflow.lazygit_command.clone(),
            diff_command: workflow.diff_command.clone(),
        };
        let source = match mapped.filter(|_| mode.checks_out()) {
            Some(repo) => WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: crate::worktree::expand_tilde_absolute_path(&repo.path),
                remote: repo.remote.clone(),
                fetch_refspec: format!("+refs/pull/{number}/head:refs/herdr/pull/{number}"),
                base_ref: format!("refs/herdr/pull/{number}"),
                branch: format!("review/pr-{number}"),
            }),
            None => WorkspaceSource::Download(DownloadSpec {
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
        Ok(ProvisionPlan {
            source,
            workspace_label,
            agent_name_hint,
            brief,
            layout,
            delete_branch: workflow.delete_branch,
        })
    }

    fn remove_on_resolved(&self, item: &WorkItem) -> bool {
        let repo = parse_external_id(&item.external_id)
            .map(|(repo, _)| repo)
            .unwrap_or(&item.external_id);
        self.workflow(repo).config.on_resolved == OnResolvedConfig::Remove
    }

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        (
            "Review requested".into(),
            Some(format!("{} · {}", item.context, item.title)),
        )
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
        let items = parse_search(json).expect("parses");
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
        assert_eq!(parse_search(json).expect("parses"), Vec::new());
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
            prepared_for: None,
            prepare_in_flight: false,
            provisioning: None,
            resolve_error: None,
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
            })
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
}
