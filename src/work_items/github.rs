//! GitHub pull request review requests, read through the GitHub CLI.

use std::collections::HashMap;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};
use crate::config::{GithubRepoConfig, GithubWorkItemsConfig};

use super::process::{failure_detail, run_with_timeout};
use super::source::{
    CheckoutSpec, ItemChoices, PreparedItem, ProvisionPlan, ServerPlan, SourceItem, WorkItemSource,
};
use super::state::WorkItem;

const SOURCE_ID: &str = "github";
const GH_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_POLL_SECONDS: u64 = 30;
const MAX_POLL_SECONDS: u64 = 3600;
const GITHUB_CHOICE_ID: &str = "github";
const LOCAL_CHOICE_ID: &str = "local";
const MAX_WORKSPACE_LABEL_CHARS: usize = 40;
const MAX_BRIEF_BODY_CHARS: usize = 4000;
const MAX_BRIEF_FILES: usize = 100;

pub(crate) struct GithubSource {
    config: GithubWorkItemsConfig,
    docs: Vec<Regex>,
    frontend: Vec<Regex>,
    repo_frontend: HashMap<String, Vec<Regex>>,
    build_error: Option<String>,
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
        let mut compile = |patterns: &[String], key: &str| {
            compile_patterns(patterns, key).unwrap_or_else(|err| {
                build_error.get_or_insert(err);
                Vec::new()
            })
        };
        let docs = compile(&config.docs_patterns, "docs_patterns");
        let frontend = compile(&config.frontend_patterns, "frontend_patterns");
        let mut repo_frontend = HashMap::new();
        for repo in &config.repos {
            if let Some(patterns) = &repo.frontend_patterns {
                repo_frontend.insert(
                    repo.name.to_ascii_lowercase(),
                    compile(patterns, "frontend_patterns"),
                );
            }
        }
        Self {
            config,
            docs,
            frontend,
            repo_frontend,
            build_error,
        }
    }

    fn repo(&self, name: &str) -> Option<&GithubRepoConfig> {
        self.config
            .repos
            .iter()
            .find(|repo| repo.name.eq_ignore_ascii_case(name))
    }

    fn frontend_patterns(&self, repo: &str) -> &[Regex] {
        self.repo_frontend
            .get(&repo.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&self.frontend)
    }

    pub(crate) fn touches_frontend(&self, repo: &str, detail: &GithubDetail) -> bool {
        let patterns = self.frontend_patterns(repo);
        detail
            .files
            .iter()
            .any(|file| patterns.iter().any(|pattern| pattern.is_match(&file.path)))
    }

    fn gh(&self) -> std::process::Command {
        let mut command = crate::noninteractive_process::command(&self.config.gh_path);
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1");
        command
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
        return LOCAL_CHOICE_ID;
    };
    let docs_only = !detail.files.is_empty()
        && detail
            .files
            .iter()
            .all(|file| docs.iter().any(|pattern| pattern.is_match(&file.path)));
    if docs_only || detail.additions + detail.deletions <= small_diff_lines {
        GITHUB_CHOICE_ID
    } else {
        LOCAL_CHOICE_ID
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

fn brief(repo: &str, item: &WorkItem, detail: &GithubDetail) -> String {
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
    format!(
        "You are reviewing GitHub pull request {repo}#{number}: {title}\n\
         Author: @{author} · {url}\n\
         Base {base} ← head {head_ref} ({head})\n\
         This directory is a detached checkout of the pull request head.\n\
         \n\
         Description:\n\
         {body}\n\
         \n\
         Changed files ({changed}, +{additions} −{deletions}):\n\
         {files}\n\
         \n\
         Inspect the change with git (for example `git diff origin/{base}...HEAD`), summarise it and wait for my instructions before changing anything.",
        number = detail.number,
        title = item.title,
        url = item.url,
        base = detail.base_ref_name,
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
            let mut context = format!("{repo} #{}", item.number);
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
        let query = format!("q={}", self.config.query);
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
        let mut summary = format!(
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
        if self.touches_frontend(repo, &detail) {
            summary.push_str(" · front-end");
        }
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
        let local = WorkItemChoiceInfo {
            choice_id: LOCAL_CHOICE_ID.into(),
            label: "Review locally".into(),
            action: WorkItemChoiceAction::ProvisionWorkspace,
            disabled_reason: (!mapped).then(|| {
                format!(
                    "No local checkout configured for {}",
                    repo.unwrap_or(&item.external_id)
                )
            }),
        };
        let github = WorkItemChoiceInfo {
            choice_id: GITHUB_CHOICE_ID.into(),
            label: "Review on GitHub".into(),
            action: WorkItemChoiceAction::OpenUrl {
                url: item.url.clone(),
            },
            disabled_reason: None,
        };
        ItemChoices {
            choices: vec![local, github],
            default_choice_id: Some(
                default_choice(
                    detail.as_ref(),
                    mapped,
                    self.config.small_diff_lines,
                    &self.docs,
                )
                .into(),
            ),
        }
    }

    fn provision_plan(
        &self,
        item: &WorkItem,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        let (repo_name, number) = parse_external_id(&item.external_id)
            .ok_or_else(|| format!("unrecognised pull request id {}", item.external_id))?;
        let repo = self
            .repo(repo_name)
            .ok_or_else(|| format!("No local checkout configured for {repo_name}"))?;
        let detail = item_detail(item).ok_or_else(|| {
            "pull request details are not available yet; try again shortly".to_string()
        })?;
        let short_name = repo_name.rsplit('/').next().unwrap_or(repo_name);
        let checkout_path = crate::worktree::default_checkout_path(
            worktree_directory,
            short_name,
            &format!("pr-{number}"),
        );
        let server = self
            .touches_frontend(repo_name, &detail)
            .then(|| repo.server_command.clone())
            .flatten()
            .map(|command| ServerPlan {
                command,
                port: repo.server_port,
            });
        let server_skip_reason = if server.is_some() {
            String::new()
        } else if self.touches_frontend(repo_name, &detail) {
            format!("no server_command configured for {repo_name}")
        } else {
            "no front-end changes".into()
        };
        Ok(ProvisionPlan {
            checkout: CheckoutSpec {
                repo_path: crate::worktree::expand_tilde_absolute_path(&repo.path),
                remote: repo.remote.clone(),
                fetch_refspec: format!("+refs/pull/{number}/head:refs/herdr/pull/{number}"),
                checkout_ref: format!("refs/herdr/pull/{number}"),
                checkout_path,
            },
            workspace_label: truncate_chars(
                &format!("#{number} {}", item.title),
                MAX_WORKSPACE_LABEL_CHARS,
            ),
            agent_name_hint: format!("review-{number}"),
            brief: brief(repo_name, item, &detail),
            install_command: repo.install_command.clone(),
            server,
            server_skip_reason,
        })
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
                    context: "o/r #12".into(),
                    author: Some("alice".into()),
                    url: "https://github.com/o/r/pull/12".into(),
                    updated_at: "2026-01-02T00:00:00Z".into(),
                },
                SourceItem {
                    external_id: "x/y#3".into(),
                    title: "WIP".into(),
                    context: "x/y #3 · draft".into(),
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
            context: format!("{repo} #5"),
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
            install_command: Some("npm ci".into()),
            server_command: Some("npm run dev".into()),
            server_port: Some(3000),
            frontend_patterns: None,
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
    fn repository_frontend_patterns_override_the_defaults() {
        let source = mapped_source(GithubRepoConfig {
            frontend_patterns: Some(vec![r"^web/".into()]),
            ..repo_config()
        });
        assert!(source.touches_frontend("o/r", &detail(&[("web/app.rs", 1, 0)])));
        assert!(!source.touches_frontend("o/r", &detail(&[("src/view.tsx", 1, 0)])));
    }

    #[test]
    fn provision_plan_checks_out_under_the_worktree_root_and_briefs_the_change() {
        let source = mapped_source(repo_config());
        let change = detail(&[("src/a.rs", 40, 2), ("src/b.rs", 1, 1)]);
        let plan = source
            .provision_plan(&work_item("o/r", Some(&change)), Path::new("/worktrees"))
            .expect("plan");
        assert_eq!(plan.checkout.checkout_path, Path::new("/worktrees/r/pr-5"));
        assert_eq!(plan.checkout.checkout_ref, "refs/herdr/pull/5");
        assert_eq!(plan.server, None);
        assert_eq!(plan.server_skip_reason, "no front-end changes");
        assert!(plan.brief.contains("Add the thing"));
        assert!(plan.brief.contains("- src/a.rs (+40 −2)"));
        assert!(plan.brief.contains("- src/b.rs (+1 −1)"));
    }

    #[test]
    fn provision_plan_starts_a_server_for_frontend_changes() {
        let source = mapped_source(repo_config());
        let change = detail(&[("web/app.tsx", 40, 2)]);
        let plan = source
            .provision_plan(&work_item("o/r", Some(&change)), Path::new("/worktrees"))
            .expect("plan");
        assert_eq!(
            plan.server,
            Some(ServerPlan {
                command: "npm run dev".into(),
                port: Some(3000),
            })
        );
    }

    #[test]
    fn invalid_pattern_is_surfaced_by_poll() {
        let source = GithubSource::new(GithubWorkItemsConfig {
            docs_patterns: vec!["(".into()],
            gh_path: "/nonexistent/gh".into(),
            ..GithubWorkItemsConfig::default()
        });
        let error = source.poll().expect_err("poll fails");
        assert!(
            error.starts_with("invalid docs_patterns pattern"),
            "{error}"
        );
    }
}
