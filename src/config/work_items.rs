use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_GITHUB_REVIEW_REQUESTED_QUERY: &str =
    "is:pr is:open review-requested:@me archived:false";
pub const DEFAULT_GITHUB_CHANGES_REQUESTED_QUERY: &str =
    "is:pr is:open author:@me review:changes_requested archived:false";
pub const DEFAULT_GITHUB_CI_FAILING_QUERY: &str =
    "is:pr is:open author:@me status:failure archived:false";
pub const DEFAULT_GITHUB_ASSIGNED_QUERY: &str = "is:issue is:open assignee:@me archived:false";
/// Candidates only: approval is read from each pull request's latest reviews, because
/// `review:approved` misses repositories that do not require reviews.
pub const DEFAULT_GITHUB_READY_TO_MERGE_QUERY: &str =
    "is:pr is:open author:@me draft:false archived:false";
/// herdr-reviewr, showing the pull request against its base.
pub const DEFAULT_REVIEW_COMMAND: &str =
    "{plugin:persiyanov.reviewr}/bin/herdr-reviewr --base {base}";

/// A tab opened after the agent's tab in a workspace with a checkout, in list order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTabConfig {
    /// Tab label.
    pub label: String,
    /// Command typed into the tab; {plugin:ID} is an installed plugin's folder. Empty skips the tab.
    pub command: String,
    /// Command used instead when `command` names a plugin that is not installed. Empty skips the tab. Default: "".
    #[serde(default)]
    pub fallback: String,
}

impl WorkspaceTabConfig {
    fn new(label: &str, command: &str, fallback: &str) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
            fallback: fallback.into(),
        }
    }
}

/// Editor and lazygit.
fn default_branch_tabs() -> Vec<WorkspaceTabConfig> {
    vec![
        WorkspaceTabConfig::new("editor", "nvim .", ""),
        WorkspaceTabConfig::new("lazygit", "lazygit", ""),
    ]
}

/// Editor and herdr-reviewr, or lazygit without the reviewr plugin.
fn default_review_tabs() -> Vec<WorkspaceTabConfig> {
    vec![
        WorkspaceTabConfig::new("editor", "nvim .", ""),
        WorkspaceTabConfig::new("review", DEFAULT_REVIEW_COMMAND, "lazygit"),
    ]
}

pub const DEFAULT_JIRA_JQL: &str =
    "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC";

/// Agent started in work-item workspaces when nothing chooses another.
pub const DEFAULT_AGENT: &str = "claude";
/// omp's slash command that switches its session into plan mode.
const OMP_PLAN_COMMAND: &str = "/plan";
/// Seconds one command may run before the agent running it needs you, unless configured.
const DEFAULT_STUCK_AFTER_SECONDS: u64 = 600;
/// The shortest and the longest `stuck_after_seconds` that is taken as set.
const MIN_STUCK_AFTER_SECONDS: u64 = 60;
const MAX_STUCK_AFTER_SECONDS: u64 = 24 * 60 * 60;

/// An agent kind and the extra arguments it is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    /// Agent kind, e.g. "claude" or "omp"; empty starts none.
    pub agent: String,
    pub args: Vec<String>,
}

impl Default for AgentLaunch {
    fn default() -> Self {
        Self {
            agent: DEFAULT_AGENT.into(),
            args: Vec::new(),
        }
    }
}

impl AgentLaunch {
    /// `agent` and `args` where set, each falling back to `self` on its own.
    fn overridden_by(&self, agent: Option<&String>, args: Option<&Vec<String>>) -> Self {
        Self {
            agent: agent.unwrap_or(&self.agent).clone(),
            args: args.unwrap_or(&self.args).clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WorkItemsConfig {
    /// Agent started in the first tab of every workspace opened from a work item, unless a workflow block or pick_next sets its own. Empty disables the agent-led choices. Default: "claude".
    pub agent: String,
    /// Extra arguments for that agent, unless a workflow block or pick_next sets its own. Default: [].
    pub agent_args: Vec<String>,
    /// The "Pick next task…" discovery workspace.
    pub pick_next: PickNextConfig,
    /// Seconds an agent may run one command before it counts as stuck and needs you, clamped to 60–86400. Default: 600.
    pub stuck_after_seconds: u64,
    /// GitHub pull requests. Unset disables the source.
    pub github: Option<GithubWorkItemsConfig>,
    /// Jira issues. Unset disables the source.
    pub jira: Option<JiraWorkItemsConfig>,
}

impl Default for WorkItemsConfig {
    fn default() -> Self {
        Self {
            agent: DEFAULT_AGENT.into(),
            agent_args: Vec::new(),
            pick_next: PickNextConfig::default(),
            stuck_after_seconds: DEFAULT_STUCK_AFTER_SECONDS,
            github: None,
            jira: None,
        }
    }
}

impl WorkItemsConfig {
    /// Environment variables the server reads credentials from on every call. A live
    /// handoff refreshes these from the caller, so a rotated token reaches the new server.
    pub fn credential_env_names(&self) -> impl Iterator<Item = &str> {
        self.jira.iter().map(|jira| jira.token_env.as_str())
    }

    /// How long an agent may run one command before it counts as stuck and needs you.
    pub fn stuck_after(&self) -> Duration {
        Duration::from_secs(
            self.stuck_after_seconds
                .clamp(MIN_STUCK_AFTER_SECONDS, MAX_STUCK_AFTER_SECONDS),
        )
    }

    /// The agent of workflow blocks that do not set their own.
    pub fn default_agent(&self) -> AgentLaunch {
        AgentLaunch {
            agent: self.agent.clone(),
            args: self.agent_args.clone(),
        }
    }

    /// The agent of the "Pick next" workspace.
    pub fn pick_next_agent(&self) -> AgentLaunch {
        self.default_agent().overridden_by(
            self.pick_next.agent.as_ref(),
            self.pick_next.agent_args.as_ref(),
        )
    }

    /// Prompt sent to the "Pick next" agent before its brief to switch it into plan mode;
    /// empty sends none.
    pub fn pick_next_plan_command(&self) -> &str {
        match &self.pick_next.plan_command {
            Some(command) => command.trim(),
            None if self
                .pick_next_agent()
                .agent
                .trim()
                .eq_ignore_ascii_case("omp") =>
            {
                OMP_PLAN_COMMAND
            }
            None => "",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PickNextConfig {
    /// Agent started in the "Pick next" workspace. Empty disables pick next. Default: work_items.agent.
    pub agent: Option<String>,
    /// Extra arguments for the "Pick next" agent. Default: work_items.agent_args.
    pub agent_args: Option<Vec<String>>,
    /// Prompt sent to the agent before the brief, to start it in plan mode; the brief then arrives as its first planning request. Empty sends none. Default: "/plan" when the agent is omp, otherwise none.
    pub plan_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JiraWorkItemsConfig {
    /// Poll this source. Default: true.
    pub enabled: bool,
    /// Jira Cloud site, e.g. "example.atlassian.net". Required.
    pub site: String,
    /// Atlassian account email used with the API token. Required.
    pub email: String,
    /// Environment variable of the Herdr server that holds the API token. Default: "ATLASSIAN_TOKEN".
    pub token_env: String,
    /// JQL selecting the issues shown in the inbox. Default: "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".
    pub jql: String,
    /// Seconds between polls, clamped to 30–3600. Default: 120.
    pub poll_interval_seconds: u64,
    /// Most issues kept from the search, fetched in pages of 100. Default: 200.
    pub max_results: usize,
    /// curl binary used for the REST API. Default: "curl".
    pub curl_path: String,
    /// Local clones that issues of a project are worked on in.
    pub projects: Vec<JiraProjectConfig>,
    /// Workflows for Jira issues; `repos` lists project keys; the first matching block applies.
    pub issues: Vec<BranchWorkflowConfig>,
}

impl Default for JiraWorkItemsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            site: String::new(),
            email: String::new(),
            token_env: "ATLASSIAN_TOKEN".into(),
            jql: DEFAULT_JIRA_JQL.into(),
            poll_interval_seconds: 120,
            max_results: 200,
            curl_path: "curl".into(),
            projects: Vec::new(),
            issues: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JiraProjectConfig {
    /// Project key, e.g. "TECH".
    pub key: String,
    /// Path of an existing local clone.
    pub path: String,
    /// Git remote the base branch is fetched from. Default: "origin".
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Branch new work starts from. Default: the remote's HEAD branch.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Name of the new branch; {key}, {key_lower} and {slug} are replaced. Default: "{key_lower}-{slug}".
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
}

fn default_branch_template() -> String {
    "{key_lower}-{slug}".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GithubWorkItemsConfig {
    /// Poll this source. Default: true.
    pub enabled: bool,
    /// GitHub CLI used for API calls and authentication. Default: "gh".
    pub gh_path: String,
    /// Seconds between polls, clamped to 30–3600. Default: 60.
    pub poll_interval_seconds: u64,
    /// Most results kept per search, fetched in pages of 100 (at most 1000). Default: 200.
    pub max_results: usize,
    /// GitHub search queries, one per event.
    pub queries: GithubQueriesConfig,
    /// Local clones of repositories that can be reviewed in a worktree.
    pub repos: Vec<GithubRepoConfig>,
    /// Directory holding your clones, e.g. "~/projects". An item of a repository without a `repos` entry then offers to link its clone there, cloning it there first when there is none. Empty disables this. Default: "".
    pub clone_root: String,
    /// Workflows for review requests; the first block whose repos match applies.
    pub review_requested: Vec<ReviewRequestedConfig>,
    /// Workflows for your pull requests with changes requested; the first block whose repos match applies.
    pub changes_requested: Vec<BranchWorkflowConfig>,
    /// Workflows for your pull requests with failing checks; the first block whose repos match applies.
    pub ci_failing: Vec<BranchWorkflowConfig>,
    /// Workflows for issues assigned to you; the first block whose repos match applies.
    pub assigned: Vec<BranchWorkflowConfig>,
    /// Workflows for issues and pull requests that mention you; the first block whose repos match applies.
    pub mentioned: Vec<BranchWorkflowConfig>,
}

impl Default for GithubWorkItemsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gh_path: "gh".into(),
            poll_interval_seconds: 60,
            max_results: 200,
            queries: GithubQueriesConfig::default(),
            repos: Vec::new(),
            clone_root: String::new(),
            review_requested: Vec::new(),
            changes_requested: Vec::new(),
            ci_failing: Vec::new(),
            assigned: Vec::new(),
            mentioned: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GithubQueriesConfig {
    /// Search query for pull requests that request your review. Empty disables the event. Default: "is:pr is:open review-requested:@me archived:false".
    pub review_requested: String,
    /// Search query for your pull requests with changes requested. Empty disables the event. Default: "is:pr is:open author:@me review:changes_requested archived:false".
    pub changes_requested: String,
    /// Search query for your pull requests with failing checks. Empty disables the event. Default: "is:pr is:open author:@me status:failure archived:false".
    pub ci_failing: String,
    /// Search query for issues assigned to you. Empty disables the event. Default: "is:issue is:open assignee:@me archived:false".
    pub assigned: String,
    /// Search query for issues and pull requests that mention you, e.g. "is:open mentions:@me". Empty disables the event. Default: "".
    pub mentioned: String,
    /// Search query for your pull requests that may be ready to merge; those approved in their latest reviews are shown. Empty disables the event. Default: "is:pr is:open author:@me draft:false archived:false".
    pub ready_to_merge: String,
}

impl Default for GithubQueriesConfig {
    fn default() -> Self {
        Self {
            review_requested: DEFAULT_GITHUB_REVIEW_REQUESTED_QUERY.into(),
            changes_requested: DEFAULT_GITHUB_CHANGES_REQUESTED_QUERY.into(),
            ci_failing: DEFAULT_GITHUB_CI_FAILING_QUERY.into(),
            assigned: DEFAULT_GITHUB_ASSIGNED_QUERY.into(),
            mentioned: String::new(),
            ready_to_merge: DEFAULT_GITHUB_READY_TO_MERGE_QUERY.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubRepoConfig {
    /// Repository as owner/name.
    pub name: String,
    /// Path of an existing local clone.
    pub path: String,
    /// Git remote name or URL that serves refs/pull/*. Default: "origin".
    #[serde(default = "default_remote")]
    pub remote: String,
}

fn default_remote() -> String {
    "origin".into()
}

/// What happens to an item's workspace once the event no longer applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnResolvedConfig {
    /// Keep the workspace; the item stays marked as reviewed until you close it.
    #[default]
    Keep,
    /// Remove the worktree and its workspace, unless the worktree has uncommitted changes.
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewRequestedConfig {
    /// Repositories (owner/name) this block applies to. Empty applies to every repository.
    pub repos: Vec<String>,
    /// What happens to the workspace once the review request is gone. Default: "keep".
    pub on_resolved: OnResolvedConfig,
    /// Delete the local review branch when its worktree is removed. Default: true.
    pub delete_branch: bool,
    /// Agent started in the first tab. Empty disables the agent-led choices. Default: work_items.agent.
    pub agent: Option<String>,
    /// Extra arguments for the agent, e.g. ["--model", "opus", "--effort", "high"] for Claude Code. Default: work_items.agent_args.
    pub agent_args: Option<Vec<String>>,
    /// Tabs after the agent's tab in a worktree review, each { label, command, fallback }; {base} is the pull request's base branch. Default: an "editor" tab running "nvim ." and a "review" tab running "{plugin:persiyanov.reviewr}/bin/herdr-reviewr --base {base}", falling back to "lazygit".
    pub tabs: Vec<WorkspaceTabConfig>,
    /// Command run in the diff tab of an agent review without checkout; {file} is the downloaded diff. Empty disables the tab. Default: "nvim -R {file}".
    pub diff_command: String,
}

impl Default for ReviewRequestedConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            on_resolved: OnResolvedConfig::Keep,
            delete_branch: true,
            agent: None,
            agent_args: None,
            tabs: default_review_tabs(),
            diff_command: "nvim -R {file}".into(),
        }
    }
}

impl ReviewRequestedConfig {
    pub fn applies_to(&self, repo: &str) -> bool {
        repo_matches(&self.repos, repo)
    }

    /// This block's agent, else `default`'s.
    pub fn agent<'a>(&'a self, default: &'a AgentLaunch) -> &'a str {
        self.agent.as_deref().unwrap_or(&default.agent)
    }

    /// This block's agent arguments, else `default`'s.
    pub fn agent_args<'a>(&'a self, default: &'a AgentLaunch) -> &'a [String] {
        self.agent_args.as_deref().unwrap_or(&default.args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BranchWorkflowConfig {
    /// Repositories (owner/name) this block applies to. Empty applies to every repository.
    pub repos: Vec<String>,
    /// What happens to the workspace once the event no longer applies. Default: "keep".
    pub on_resolved: OnResolvedConfig,
    /// Delete the local branch when a worktree created for it is removed. Default: false.
    pub delete_branch: bool,
    /// Agent started in the first tab. Empty disables the agent-led choices. Default: work_items.agent.
    pub agent: Option<String>,
    /// Extra arguments for the agent, e.g. ["--model", "opus", "--effort", "high"] for Claude Code. Default: work_items.agent_args.
    pub agent_args: Option<Vec<String>>,
    /// Tabs after the agent's tab in a workspace with a checkout, each { label, command, fallback }. Default: an "editor" tab running "nvim ." and a "lazygit" tab running "lazygit".
    pub tabs: Vec<WorkspaceTabConfig>,
    /// Command showing a downloaded thread when there is no checkout; {file} is the file. Empty disables the tab. Default: "nvim -R {file}".
    pub viewer_command: String,
}

impl Default for BranchWorkflowConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            on_resolved: OnResolvedConfig::Keep,
            delete_branch: false,
            agent: None,
            agent_args: None,
            tabs: default_branch_tabs(),
            viewer_command: "nvim -R {file}".into(),
        }
    }
}

impl BranchWorkflowConfig {
    pub fn applies_to(&self, repo: &str) -> bool {
        repo_matches(&self.repos, repo)
    }

    /// This block's agent, else `default`'s.
    pub fn agent<'a>(&'a self, default: &'a AgentLaunch) -> &'a str {
        self.agent.as_deref().unwrap_or(&default.agent)
    }

    /// This block's agent arguments, else `default`'s.
    pub fn agent_args<'a>(&'a self, default: &'a AgentLaunch) -> &'a [String] {
        self.agent_args.as_deref().unwrap_or(&default.args)
    }
}

fn repo_matches(repos: &[String], repo: &str) -> bool {
    repos.is_empty()
        || repos
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(repo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn default_config_has_no_github_source() {
        assert_eq!(Config::default().work_items.github, None);
    }

    fn parse(toml: &str) -> GithubWorkItemsConfig {
        let config: Config = toml::from_str(toml).expect("config parses");
        config.work_items.github.expect("github source configured")
    }

    #[test]
    fn github_section_with_repo_mapping_parses_with_defaults() {
        let github = parse(
            r#"
[work_items.github]

[[work_items.github.repos]]
name = "owner/repo"
path = "~/src/repo"
"#,
        );
        assert!(github.enabled);
        assert_eq!(
            github.queries.review_requested,
            DEFAULT_GITHUB_REVIEW_REQUESTED_QUERY
        );
        assert_eq!(github.repos[0].remote, "origin");
        assert!(github.review_requested.is_empty());
        assert_eq!(
            github.queries.changes_requested,
            DEFAULT_GITHUB_CHANGES_REQUESTED_QUERY
        );
    }

    #[test]
    fn review_requested_blocks_parse_with_repo_filters() {
        let github = parse(
            r#"
[work_items.github]

[[work_items.github.review_requested]]
repos = ["acme/app"]
on_resolved = "remove"
delete_branch = false

[[work_items.github.review_requested]]
agent = ""
"#,
        );
        let [app, fallback] = github.review_requested.as_slice() else {
            panic!("two blocks");
        };
        assert!(app.applies_to("Acme/App"));
        assert!(!app.applies_to("acme/other"));
        assert_eq!(app.on_resolved, OnResolvedConfig::Remove);
        assert!(!app.delete_branch);
        assert!(fallback.applies_to("acme/other"));
        assert_eq!(fallback.on_resolved, OnResolvedConfig::Keep);
        // An empty agent set on the block beats the default.
        assert_eq!(fallback.agent(&AgentLaunch::default()), "");
    }

    #[test]
    fn global_agent_reaches_every_workflow_and_pick_next_unless_overridden() {
        let config: Config = toml::from_str(
            r#"
[work_items]
agent = "omp"
agent_args = ["--model", "x"]

[work_items.github]
[[work_items.github.review_requested]]
repos = ["acme/app"]
[[work_items.github.review_requested]]
agent = "claude"

[work_items.jira]
[[work_items.jira.issues]]
agent_args = []
"#,
        )
        .expect("config parses");
        let work_items = config.work_items;
        let default = work_items.default_agent();
        let github = work_items.github.as_ref().expect("github configured");
        let [inherits, own] = github.review_requested.as_slice() else {
            panic!("two blocks");
        };
        assert_eq!(inherits.agent(&default), "omp");
        assert_eq!(inherits.agent_args(&default), ["--model", "x"]);
        assert_eq!(own.agent(&default), "claude");
        let jira = &work_items.jira.as_ref().expect("jira configured").issues[0];
        assert_eq!(jira.agent(&default), "omp");
        assert!(jira.agent_args(&default).is_empty());
        assert_eq!(
            work_items.pick_next_agent(),
            AgentLaunch {
                agent: "omp".into(),
                args: vec!["--model".into(), "x".into()],
            }
        );
        assert_eq!(work_items.pick_next_plan_command(), "/plan");
    }

    #[test]
    fn pick_next_settings_override_the_defaults_and_plan_mode() {
        let parse = |toml: &str| -> WorkItemsConfig {
            toml::from_str::<Config>(toml)
                .expect("config parses")
                .work_items
        };
        let claude = parse("[work_items.pick_next]\nagent_args = [\"--model\", \"opus\"]\n");
        assert_eq!(claude.pick_next_agent().agent, "claude");
        assert_eq!(claude.pick_next_agent().args, ["--model", "opus"]);
        assert_eq!(claude.pick_next_plan_command(), "");

        let omp =
            parse("[work_items]\nagent = \"claude\"\n[work_items.pick_next]\nagent = \"omp\"\n");
        assert_eq!(omp.default_agent().agent, "claude");
        assert_eq!(omp.pick_next_agent().agent, "omp");
        assert_eq!(omp.pick_next_plan_command(), "/plan");

        let no_plan =
            parse("[work_items]\nagent = \"omp\"\n[work_items.pick_next]\nplan_command = \"\"\n");
        assert_eq!(no_plan.pick_next_plan_command(), "");
        let custom = parse("[work_items.pick_next]\nplan_command = \"/plan-mode\"\n");
        assert_eq!(custom.pick_next_plan_command(), "/plan-mode");
    }

    #[test]
    fn an_agent_is_stuck_after_ten_minutes_on_one_command_unless_configured() {
        let stuck_after = |toml: &str| {
            toml::from_str::<Config>(toml)
                .expect("config parses")
                .work_items
                .stuck_after()
        };
        assert_eq!(stuck_after(""), Duration::from_secs(600));
        assert_eq!(
            stuck_after("[work_items]\nstuck_after_seconds = 90\n"),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn stuck_after_is_kept_between_a_minute_and_a_day() {
        let stuck_after = |seconds: u64| {
            WorkItemsConfig {
                stuck_after_seconds: seconds,
                ..WorkItemsConfig::default()
            }
            .stuck_after()
        };
        assert_eq!(stuck_after(0), Duration::from_secs(60));
        assert_eq!(stuck_after(59), Duration::from_secs(60));
        assert_eq!(stuck_after(86_400), Duration::from_secs(86_400));
        assert_eq!(stuck_after(86_401), Duration::from_secs(86_400));
        assert_eq!(stuck_after(u64::MAX), Duration::from_secs(86_400));
    }

    #[test]
    fn changes_requested_blocks_keep_branches_by_default() {
        let github = parse(
            r#"
[work_items.github]

[[work_items.github.changes_requested]]
repos = ["acme/app"]
"#,
        );
        let [block] = github.changes_requested.as_slice() else {
            panic!("one block");
        };
        assert!(block.applies_to("acme/app"));
        assert!(!block.delete_branch);
        assert_eq!(block.on_resolved, OnResolvedConfig::Keep);
    }

    #[test]
    fn workflow_tabs_replace_the_default_layout_in_order() {
        let config: Config = toml::from_str(
            r#"
[work_items.jira]
site = "acme.atlassian.net"
email = "me@acme.test"

[[work_items.jira.issues]]
tabs = [
  { label = "review", command = "{plugin:p}/bin/r", fallback = "lazygit" },
  { label = "editor", command = "hx ." },
]
"#,
        )
        .expect("config parses");
        let jira = config.work_items.jira.expect("jira configured");
        assert_eq!(
            jira.issues[0].tabs,
            [
                WorkspaceTabConfig::new("review", "{plugin:p}/bin/r", "lazygit"),
                WorkspaceTabConfig::new("editor", "hx .", ""),
            ]
        );
        let review_tab = |key: &str| {
            toml::from_str::<Config>(&format!(
                "[work_items.github]\n[[work_items.github.review_requested]]\n\
                 tabs = [{{ label = \"editor\", {key} = \"hx .\" }}]\n"
            ))
        };
        assert!(review_tab("command").is_ok());
        assert!(review_tab("cmd").is_err(), "a misspelt key is rejected");
    }
}
