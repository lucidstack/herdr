use serde::Deserialize;

pub const DEFAULT_GITHUB_REVIEW_REQUESTED_QUERY: &str =
    "is:pr is:open review-requested:@me archived:false";
pub const DEFAULT_GITHUB_CHANGES_REQUESTED_QUERY: &str =
    "is:pr is:open author:@me review:changes_requested archived:false";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WorkItemsConfig {
    /// GitHub pull requests. Unset disables the source.
    pub github: Option<GithubWorkItemsConfig>,
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
    /// GitHub search queries, one per event.
    pub queries: GithubQueriesConfig,
    /// Local clones of repositories that can be reviewed in a worktree.
    pub repos: Vec<GithubRepoConfig>,
    /// Workflows for review requests; the first block whose repos match applies.
    pub review_requested: Vec<ReviewRequestedConfig>,
    /// Workflows for your pull requests with changes requested; the first block whose repos match applies.
    pub changes_requested: Vec<ChangesRequestedConfig>,
}

impl Default for GithubWorkItemsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gh_path: "gh".into(),
            poll_interval_seconds: 60,
            queries: GithubQueriesConfig::default(),
            repos: Vec::new(),
            review_requested: Vec::new(),
            changes_requested: Vec::new(),
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
}

impl Default for GithubQueriesConfig {
    fn default() -> Self {
        Self {
            review_requested: DEFAULT_GITHUB_REVIEW_REQUESTED_QUERY.into(),
            changes_requested: DEFAULT_GITHUB_CHANGES_REQUESTED_QUERY.into(),
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
    /// Pull requests with at most this many changed lines default to reviewing on GitHub. Default: 20.
    pub small_diff_lines: u64,
    /// Regexes matched against changed paths; documentation-only pull requests default to reviewing on GitHub.
    pub docs_patterns: Vec<String>,
    /// Agent started in the first tab. Empty disables the agent-led choices. Default: "claude".
    pub agent: String,
    /// Command run in the editor tab of a worktree review. Empty disables the tab. Default: "nvim .".
    pub editor_command: String,
    /// Command run in the Git tab of a worktree review. Empty disables the tab. Default: "lazygit".
    pub lazygit_command: String,
    /// Command run in the diff tab of an agent review without checkout; {file} is the downloaded diff. Empty disables the tab. Default: "nvim -R {file}".
    pub diff_command: String,
}

impl Default for ReviewRequestedConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            on_resolved: OnResolvedConfig::Keep,
            delete_branch: true,
            small_diff_lines: 20,
            docs_patterns: vec![
                r"\.(md|mdx|markdown|rst|txt|adoc)$".into(),
                r"^docs/".into(),
            ],
            agent: "claude".into(),
            editor_command: "nvim .".into(),
            lazygit_command: "lazygit".into(),
            diff_command: "nvim -R {file}".into(),
        }
    }
}

impl ReviewRequestedConfig {
    pub fn applies_to(&self, repo: &str) -> bool {
        repo_matches(&self.repos, repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ChangesRequestedConfig {
    /// Repositories (owner/name) this block applies to. Empty applies to every repository.
    pub repos: Vec<String>,
    /// What happens to the workspace once changes are no longer requested. Default: "keep".
    pub on_resolved: OnResolvedConfig,
    /// Delete the local branch when a worktree created for it is removed. Default: false.
    pub delete_branch: bool,
    /// Agent started in the first tab. Empty disables the agent-led choice. Default: "claude".
    pub agent: String,
    /// Command run in the editor tab. Empty disables the tab. Default: "nvim .".
    pub editor_command: String,
    /// Command run in the Git tab. Empty disables the tab. Default: "lazygit".
    pub lazygit_command: String,
}

impl Default for ChangesRequestedConfig {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            on_resolved: OnResolvedConfig::Keep,
            delete_branch: false,
            agent: "claude".into(),
            editor_command: "nvim .".into(),
            lazygit_command: "lazygit".into(),
        }
    }
}

impl ChangesRequestedConfig {
    pub fn applies_to(&self, repo: &str) -> bool {
        repo_matches(&self.repos, repo)
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
        assert_eq!(fallback.agent, "");
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
}
