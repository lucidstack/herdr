use serde::Deserialize;

pub const DEFAULT_GITHUB_WORK_ITEMS_QUERY: &str =
    "is:pr is:open review-requested:@me archived:false";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WorkItemsConfig {
    /// GitHub pull request review requests. Unset disables the source.
    pub github: Option<GithubWorkItemsConfig>,
    /// Workspace layout used when an item is reviewed locally.
    pub workspace: WorkItemWorkspaceConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GithubWorkItemsConfig {
    /// Poll this source. Default: true.
    pub enabled: bool,
    /// GitHub CLI used for API calls and authentication. Default: "gh".
    pub gh_path: String,
    /// GitHub search query. Default: "is:pr is:open review-requested:@me archived:false".
    pub query: String,
    /// Seconds between polls, clamped to 30–3600. Default: 60.
    pub poll_interval_seconds: u64,
    /// Pull requests with at most this many changed lines default to reviewing on GitHub. Default: 20.
    pub small_diff_lines: u64,
    /// Regexes matched against changed paths; documentation-only pull requests default to reviewing on GitHub.
    pub docs_patterns: Vec<String>,
    /// Regexes matched against changed paths that mark a pull request as touching front-end code.
    pub frontend_patterns: Vec<String>,
    /// Local checkouts of repositories that can be reviewed locally.
    pub repos: Vec<GithubRepoConfig>,
}

impl Default for GithubWorkItemsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gh_path: "gh".into(),
            query: DEFAULT_GITHUB_WORK_ITEMS_QUERY.into(),
            poll_interval_seconds: 60,
            small_diff_lines: 20,
            docs_patterns: vec![
                r"\.(md|mdx|markdown|rst|txt|adoc)$".into(),
                r"^docs/".into(),
            ],
            frontend_patterns: vec![r"\.(tsx|jsx|vue|svelte|css|scss|sass|less|html)$".into()],
            repos: Vec::new(),
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
    /// Command run in the worktree to install dependencies.
    #[serde(default)]
    pub install_command: Option<String>,
    /// Command that starts a development server for front-end changes.
    #[serde(default)]
    pub server_command: Option<String>,
    /// Local port the development server listens on; enables the "server running" check.
    #[serde(default)]
    pub server_port: Option<u16>,
    /// Overrides frontend_patterns for this repository.
    #[serde(default)]
    pub frontend_patterns: Option<Vec<String>>,
}

fn default_remote() -> String {
    "origin".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WorkItemWorkspaceConfig {
    /// Agent started and briefed in the first tab. Empty disables it. Default: "claude".
    pub agent: String,
    /// Command run in the editor tab. Empty disables the tab. Default: "nvim .".
    pub editor_command: String,
    /// Command run in the Git tab. Empty disables the tab. Default: "lazygit".
    pub lazygit_command: String,
    /// Command run in the editor tab of an agent review without checkout; {file} is the downloaded diff. Empty disables the tab. Default: "nvim -R {file}".
    pub diff_command: String,
}

impl Default for WorkItemWorkspaceConfig {
    fn default() -> Self {
        Self {
            agent: "claude".into(),
            editor_command: "nvim .".into(),
            lazygit_command: "lazygit".into(),
            diff_command: "nvim -R {file}".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn default_config_has_no_github_source() {
        assert_eq!(Config::default().work_items.github, None);
    }

    #[test]
    fn github_section_with_repo_mapping_parses_with_defaults() {
        let config: Config = toml::from_str(
            r#"
[work_items.github]
query = "is:pr"

[[work_items.github.repos]]
name = "owner/repo"
path = "~/src/repo"
"#,
        )
        .expect("config parses");
        let github = config.work_items.github.expect("github source configured");
        assert_eq!(github.query, "is:pr");
        assert!(github.enabled);
        assert_eq!(github.repos.len(), 1);
        assert_eq!(github.repos[0].remote, "origin");
        assert_eq!(
            github.docs_patterns,
            GithubWorkItemsConfig::default().docs_patterns
        );
        assert_eq!(
            github.frontend_patterns,
            GithubWorkItemsConfig::default().frontend_patterns
        );
    }
}
