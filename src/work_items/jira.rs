//! Jira Cloud issues through the REST API, read with curl.
//!
//! The API token is read from the server's environment and handed to curl on stdin, so it
//! never appears in a process list. Classic tokens work against the site; scoped tokens only
//! through the `api.atlassian.com` gateway, which is tried when the site refuses the token.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};
use crate::config::{
    BranchWorkflowConfig, JiraProjectConfig, JiraWorkItemsConfig, OnResolvedConfig,
};

use super::github::{one_line, slug, truncate_chars};
use super::process::{failure_detail, run_with_input, run_with_timeout};
use super::provision::find_existing_work;
use super::source::{
    ItemChoices, PreparedItem, ProvisionPlan, SourceItem, WorkItemSource, WorkspaceLayout,
    WorkspaceSource, WorktreeSpec,
};
use super::state::WorkItem;

const SOURCE_ID: &str = "jira";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_POLL_SECONDS: u64 = 30;
const MAX_POLL_SECONDS: u64 = 3600;
const PAGE_SIZE: usize = 100;
const MAX_BODY_CHARS: usize = 4000;
const MAX_COMMENTS: usize = 30;
const MAX_LABEL_CHARS: usize = 40;
const JIRA_CHOICE_ID: &str = "jira";
const LOCAL_CHOICE_ID: &str = "local";
const AGENT_CHOICE_ID: &str = "local_agent";

pub(crate) struct JiraSource {
    config: JiraWorkItemsConfig,
    fallback: BranchWorkflowConfig,
    build_error: Option<String>,
    /// API base that accepted the token: the site or the gateway.
    api_base: Mutex<Option<String>>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    issues: Vec<SearchIssue>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct SearchIssue {
    key: String,
    fields: SearchFields,
}

#[derive(Deserialize)]
struct SearchFields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    updated: String,
    #[serde(default)]
    status: Option<Named>,
    #[serde(default)]
    reporter: Option<Person>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Person {
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Deserialize)]
struct IssueResponse {
    fields: IssueFields,
}

#[derive(Deserialize)]
struct IssueFields {
    #[serde(default)]
    description: Option<serde_json::Value>,
    #[serde(default)]
    issuetype: Option<Named>,
    #[serde(default)]
    priority: Option<Named>,
    #[serde(default)]
    status: Option<Named>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    comment: Option<CommentPage>,
}

#[derive(Deserialize)]
struct CommentPage {
    #[serde(default)]
    comments: Vec<RestComment>,
}

#[derive(Deserialize)]
struct RestComment {
    #[serde(default)]
    author: Option<Person>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct TenantInfo {
    #[serde(rename = "cloudId")]
    cloud_id: String,
}

/// Issue details kept as the item's detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JiraDetail {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Oldest first.
    #[serde(default)]
    pub comments: Vec<JiraComment>,
    /// Branch new work starts from, when the project is mapped.
    #[serde(default)]
    pub base_branch: String,
    /// A local branch (with its worktree, when checked out) that already names the issue.
    #[serde(default)]
    pub existing_branch: Option<String>,
    #[serde(default)]
    pub existing_worktree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JiraComment {
    pub author: String,
    pub body: String,
}

/// Plain text of an Atlassian Document Format node.
fn adf_text(node: &serde_json::Value) -> String {
    let mut out = String::new();
    write_adf(node, &mut out);
    out.trim().to_string()
}

fn write_adf(node: &serde_json::Value, out: &mut String) {
    let kind = node
        .get("type")
        .and_then(|kind| kind.as_str())
        .unwrap_or("");
    let attr = |name: &str| {
        node.get("attrs")
            .and_then(|attrs| attrs.get(name))
            .and_then(|value| value.as_str())
    };
    match kind {
        "text" => out.push_str(
            node.get("text")
                .and_then(|text| text.as_str())
                .unwrap_or(""),
        ),
        "hardBreak" => out.push('\n'),
        "mention" | "emoji" => out.push_str(attr("text").unwrap_or("")),
        "inlineCard" | "blockCard" => out.push_str(attr("url").unwrap_or("")),
        "listItem" => out.push_str("- "),
        _ => {}
    }
    if let Some(children) = node.get("content").and_then(|content| content.as_array()) {
        for child in children {
            write_adf(child, out);
        }
    }
    if matches!(
        kind,
        "paragraph" | "heading" | "listItem" | "codeBlock" | "blockquote" | "rule"
    ) {
        out.push('\n');
    }
}

/// Branch name from a project's template.
fn branch_name(template: &str, key: &str, summary: &str) -> String {
    let slug = slug(summary);
    let name = template
        .replace("{key}", key)
        .replace("{key_lower}", &key.to_ascii_lowercase())
        .replace("{slug}", &slug);
    name.trim_end_matches(['-', '/']).to_string()
}

/// Escapes a value for a double-quoted curl config parameter.
fn curl_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn project_key(issue_key: &str) -> &str {
    issue_key
        .split_once('-')
        .map_or(issue_key, |(project, _)| project)
}

fn item_detail(item: &WorkItem) -> Option<JiraDetail> {
    item.detail
        .clone()
        .and_then(|value| serde_json::from_value(value).ok())
}

impl JiraSource {
    pub(crate) fn new(config: JiraWorkItemsConfig) -> Self {
        let build_error = if config.site.trim().is_empty() {
            Some("set work_items.jira.site, e.g. \"example.atlassian.net\"".to_string())
        } else if config.email.trim().is_empty() {
            Some("set work_items.jira.email to the account the API token belongs to".to_string())
        } else {
            None
        };
        Self {
            config,
            fallback: BranchWorkflowConfig::default(),
            build_error,
            api_base: Mutex::new(None),
        }
    }

    fn site(&self) -> &str {
        self.config
            .site
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches('/')
    }

    fn browse_url(&self, key: &str) -> String {
        format!("https://{}/browse/{key}", self.site())
    }

    fn project(&self, key: &str) -> Option<&JiraProjectConfig> {
        self.config
            .projects
            .iter()
            .find(|project| project.key.eq_ignore_ascii_case(key))
    }

    fn workflow(&self, project: &str) -> &BranchWorkflowConfig {
        self.config
            .issues
            .iter()
            .find(|block| block.applies_to(project))
            .unwrap_or(&self.fallback)
    }

    fn token(&self) -> Result<String, String> {
        std::env::var(&self.config.token_env)
            .ok()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "{} is not set in the Herdr server's environment (work_items.jira.token_env)",
                    self.config.token_env
                )
            })
    }

    /// One HTTP request through curl; returns the status code and body.
    fn http(&self, method: &str, url: &str, body: Option<&str>) -> Result<(u16, Vec<u8>), String> {
        let token = self.token()?;
        let mut config = format!(
            "user = {}\nurl = {}\nrequest = {}\nheader = \"Accept: application/json\"\n",
            curl_quote(&format!("{}:{token}", self.config.email.trim())),
            curl_quote(url),
            curl_quote(method),
        );
        if let Some(body) = body {
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!("data-binary = {}\n", curl_quote(body)));
        }
        let mut command = crate::noninteractive_process::command(&self.config.curl_path);
        command.args([
            "--silent",
            "--show-error",
            "--config",
            "-",
            "--write-out",
            "\n%{http_code}",
        ]);
        let output = match run_with_input(command, config.into_bytes(), HTTP_TIMEOUT) {
            Ok(output) if output.status.success() => output,
            Ok(output) => return Err(format!("curl failed: {}", failure_detail(&output))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err("curl not found; set work_items.jira.curl_path".into())
            }
            Err(err) => return Err(format!("curl failed: {err}")),
        };
        let mut stdout = output.stdout;
        let split = stdout
            .iter()
            .rposition(|byte| *byte == b'\n')
            .ok_or("unexpected curl output")?;
        let status = std::str::from_utf8(&stdout[split + 1..])
            .ok()
            .and_then(|code| code.trim().parse().ok())
            .ok_or("unexpected curl output")?;
        stdout.truncate(split);
        Ok((status, stdout))
    }

    fn gateway_base(&self) -> Result<String, String> {
        let mut command = crate::noninteractive_process::command(&self.config.curl_path);
        command.args([
            "--silent",
            "--show-error",
            "--fail",
            &format!("https://{}/_edge/tenant_info", self.site()),
        ]);
        let output = run_with_timeout(command, HTTP_TIMEOUT).map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(failure_detail(&output));
        }
        let tenant: TenantInfo = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("unexpected tenant info: {err}"))?;
        Ok(format!(
            "https://api.atlassian.com/ex/jira/{}",
            tenant.cloud_id
        ))
    }

    /// The API base that accepts the token, found once by asking who we are. Other
    /// endpoints are unusable for this: Jira answers a search with a refused token as an
    /// anonymous user, with an empty result and HTTP 200.
    fn api_base(&self) -> Result<String, String> {
        let mut cached = self
            .api_base
            .lock()
            .map_err(|_| "Jira state is unavailable")?;
        if let Some(base) = cached.as_ref() {
            return Ok(base.clone());
        }
        let mut last_status = 0;
        for gateway in [false, true] {
            let base = if gateway {
                self.gateway_base()?
            } else {
                format!("https://{}", self.site())
            };
            let (status, response) =
                self.http("GET", &format!("{base}/rest/api/3/myself"), None)?;
            if (200..300).contains(&status) {
                *cached = Some(base.clone());
                return Ok(base);
            }
            last_status = status;
            if !matches!(status, 401 | 403) {
                return Err(http_error(status, &response));
            }
        }
        Err(match last_status {
            401 => "Jira refused the API token; check work_items.jira.email and the token".into(),
            403 => "the Jira API token lacks permission (read:jira-user, read:jira-work)".into(),
            status => format!("Jira returned HTTP {status}"),
        })
    }

    /// A REST call under `/rest/api/3`.
    fn api(&self, method: &str, path: &str, body: Option<&str>) -> Result<Vec<u8>, String> {
        let base = self.api_base()?;
        let (status, response) = self.http(method, &format!("{base}/rest/api/3/{path}"), body)?;
        if (200..300).contains(&status) {
            return Ok(response);
        }
        if matches!(status, 401 | 403) {
            // The token may have been rotated; find the base again next time.
            if let Ok(mut cached) = self.api_base.lock() {
                *cached = None;
            }
        }
        Err(http_error(status, &response))
    }

    /// Branch new work starts from: configured, else the remote's HEAD.
    fn base_branch(&self, project: &JiraProjectConfig) -> Result<String, String> {
        if let Some(base) = project.base_branch.as_ref().filter(|base| !base.is_empty()) {
            return Ok(base.clone());
        }
        let path = crate::worktree::expand_tilde_absolute_path(&project.path);
        let mut command = crate::noninteractive_process::command("git");
        command.arg("-C").arg(&path).args([
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{}/HEAD", project.remote),
        ]);
        let output = run_with_timeout(command, GIT_TIMEOUT).map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "cannot tell the base branch of {}; set base_branch for project {}",
                path.display(),
                project.key
            ));
        }
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(head
            .strip_prefix(&format!("{}/", project.remote))
            .unwrap_or(&head)
            .to_string())
    }
}

fn http_error(status: u16, body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Errors {
        #[serde(rename = "errorMessages", default)]
        error_messages: Vec<String>,
    }
    let detail = serde_json::from_slice::<Errors>(body)
        .ok()
        .and_then(|errors| errors.error_messages.into_iter().next());
    match detail {
        Some(detail) => format!("Jira returned HTTP {status}: {detail}"),
        None => format!("Jira returned HTTP {status}"),
    }
}

/// One page of search results as items, and the token of the next page.
/// `browse_url` turns an issue key into its web page.
fn parse_search(
    bytes: &[u8],
    browse_url: impl Fn(&str) -> String,
) -> Result<(Vec<SourceItem>, Option<String>), String> {
    let response: SearchResponse =
        serde_json::from_slice(bytes).map_err(|err| format!("unexpected Jira response: {err}"))?;
    let next = response.next_page_token;
    let items = response
        .issues
        .into_iter()
        .map(|issue| {
            let status = issue
                .fields
                .status
                .map(|status| status.name)
                .unwrap_or_default();
            SourceItem {
                context: if status.is_empty() {
                    issue.key.clone()
                } else {
                    format!("{} · {status}", issue.key)
                },
                url: browse_url(&issue.key),
                external_id: issue.key,
                title: issue.fields.summary,
                author: issue
                    .fields
                    .reporter
                    .map(|person| person.display_name)
                    .filter(|name| !name.is_empty()),
                updated_at: issue.fields.updated,
            }
        })
        .collect();
    Ok((items, next))
}

fn brief(item: &WorkItem, detail: &JiraDetail, agent_starts: bool, branch: &str) -> String {
    let description = if detail.description.is_empty() {
        "(no description)".to_string()
    } else {
        truncate_chars(&detail.description, MAX_BODY_CHARS)
    };
    let skipped = detail.comments.len().saturating_sub(MAX_COMMENTS);
    let mut comments: Vec<String> = detail
        .comments
        .iter()
        .skip(skipped)
        .map(|comment| format!("- {}: {}", comment.author, one_line(&comment.body)))
        .collect();
    if skipped > 0 {
        comments.insert(0, format!("- … {skipped} older comments"));
    }
    let comments = if comments.is_empty() {
        "(no comments)".to_string()
    } else {
        comments.join("\n")
    };
    let labels = if detail.labels.is_empty() {
        String::new()
    } else {
        format!(" · labels {}", detail.labels.join(", "))
    };
    let instructions = if agent_starts {
        "Implement it now, with tests. Do not commit, push or change the Jira issue. Report \
         what you changed and any open question."
    } else {
        "Investigate the code, propose an implementation plan and wait for my instructions \
         before changing anything."
    };
    format!(
        "You are working on Jira issue {key}: {title}\n\
         {url}\n\
         {kind} · {priority} · {status}{labels}\n\
         This directory is a worktree on the branch {branch} (work starts from {base}; a branch \
         you already had keeps its commits).\n\
         \n\
         Description:\n\
         {description}\n\
         \n\
         Comments:\n\
         {comments}\n\
         \n\
         {instructions}",
        key = item.external_id,
        title = item.title,
        url = item.url,
        kind = detail.issue_type,
        priority = detail.priority,
        status = detail.status,
        base = detail.base_branch,
    )
}

impl WorkItemSource for JiraSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn label(&self) -> &str {
        "Jira"
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
        let limit = self.config.max_results.max(1);
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut body = serde_json::json!({
                "jql": self.config.jql,
                "maxResults": (limit - items.len()).min(PAGE_SIZE),
                "fields": ["summary", "status", "updated", "reporter"],
            });
            if let Some(token) = page_token.take() {
                body["nextPageToken"] = serde_json::Value::String(token);
            }
            let response = self.api("POST", "search/jql", Some(&body.to_string()))?;
            let (found, next) = parse_search(&response, |key| self.browse_url(key))?;
            let empty = found.is_empty();
            items.extend(found);
            match next {
                Some(token) if !empty && items.len() < limit => page_token = Some(token),
                _ => break,
            }
        }
        items.truncate(limit);
        Ok(items)
    }

    fn prepare(&self, item: &SourceItem) -> PreparedItem {
        let fields = "description,issuetype,priority,status,labels,comment";
        let issue = self
            .api(
                "GET",
                &format!("issue/{}?fields={fields}", item.external_id),
                None,
            )
            .and_then(|bytes| {
                serde_json::from_slice::<IssueResponse>(&bytes)
                    .map_err(|err| format!("unexpected Jira response: {err}"))
            });
        let fields = match issue {
            Ok(issue) => issue.fields,
            Err(error) => {
                return PreparedItem {
                    waiting: false,
                    detail: None,
                    summary: None,
                    error: Some(error),
                }
            }
        };
        let mut error = None;
        let project = self.project(project_key(&item.external_id));
        let base_branch = match project {
            Some(project) => self.base_branch(project).unwrap_or_else(|err| {
                error = Some(err);
                String::new()
            }),
            None => String::new(),
        };
        // Shown in the dialog; the worktree worker looks again when a choice is made.
        let (existing_branch, existing_worktree) = project
            .and_then(|project| {
                let path = crate::worktree::expand_tilde_absolute_path(&project.path);
                let base_rev = format!("{}/{base_branch}", project.remote);
                find_existing_work(&path, &item.external_id, &base_branch, &base_rev)
                    .ok()
                    .flatten()
            })
            .map_or((None, None), |(branch, path)| {
                (Some(branch), path.map(|path| path.display().to_string()))
            });
        let name = |named: Option<Named>| named.map(|named| named.name).unwrap_or_default();
        let detail = JiraDetail {
            description: fields
                .description
                .as_ref()
                .map(adf_text)
                .unwrap_or_default(),
            issue_type: name(fields.issuetype),
            priority: name(fields.priority),
            status: name(fields.status),
            labels: fields.labels,
            comments: fields
                .comment
                .map(|page| page.comments)
                .unwrap_or_default()
                .into_iter()
                .map(|comment| JiraComment {
                    author: comment
                        .author
                        .map(|person| person.display_name)
                        .unwrap_or_else(|| "unknown".into()),
                    body: comment.body.as_ref().map(adf_text).unwrap_or_default(),
                })
                .collect(),
            base_branch,
            existing_branch,
            existing_worktree,
        };
        let summary = [
            detail.issue_type.clone(),
            detail.priority.clone(),
            format!(
                "{} {}",
                detail.comments.len(),
                if detail.comments.len() == 1 {
                    "comment"
                } else {
                    "comments"
                }
            ),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        PreparedItem {
            waiting: false,
            detail: serde_json::to_value(&detail).ok(),
            summary: Some(summary),
            error,
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        let project = project_key(&item.external_id);
        let unmapped = self
            .project(project)
            .is_none()
            .then(|| format!("No local checkout configured for Jira project {project}"));
        let no_agent = self
            .workflow(project)
            .agent
            .is_empty()
            .then(|| format!("No agent configured for Jira project {project}"));
        let existing = item_detail(item).and_then(|detail| {
            detail
                .existing_branch
                .map(|branch| (branch, detail.existing_worktree))
        });
        let (local_label, where_) = match &existing {
            Some((branch, Some(_))) => (
                format!("Continue on {branch}"),
                "Reopens your worktree".to_string(),
            ),
            Some((branch, None)) => (
                format!("Continue on {branch}"),
                "Worktree on your existing branch".to_string(),
            ),
            None => (
                "Work on it locally".to_string(),
                "Worktree on a new branch".to_string(),
            ),
        };
        let choices = vec![
            WorkItemChoiceInfo {
                choice_id: LOCAL_CHOICE_ID.into(),
                label: local_label,
                description: Some(format!("{where_}; the agent proposes a plan and waits")),
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: unmapped.clone(),
                confirm: None,
            },
            WorkItemChoiceInfo {
                choice_id: AGENT_CHOICE_ID.into(),
                label: "Ask agent to implement it".into(),
                description: Some(format!(
                    "{where_}; the agent implements it, no commit or push"
                )),
                action: WorkItemChoiceAction::ProvisionWorkspace,
                disabled_reason: unmapped.clone().or(no_agent),
                confirm: None,
            },
            WorkItemChoiceInfo {
                choice_id: JIRA_CHOICE_ID.into(),
                label: "Open in Jira".into(),
                description: Some("Open the issue in the browser".into()),
                action: WorkItemChoiceAction::OpenUrl {
                    url: item.url.clone(),
                },
                disabled_reason: None,
                confirm: None,
            },
        ];
        ItemChoices {
            choices,
            default_choice_id: Some(
                if unmapped.is_some() {
                    JIRA_CHOICE_ID
                } else {
                    LOCAL_CHOICE_ID
                }
                .into(),
            ),
        }
    }

    fn provision_plan(
        &self,
        item: &WorkItem,
        choice_id: &str,
        _worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        let agent_starts = match choice_id {
            LOCAL_CHOICE_ID => false,
            AGENT_CHOICE_ID => true,
            _ => return Err(format!("choice {choice_id} does not provision a workspace")),
        };
        let key = &item.external_id;
        let project_name = project_key(key);
        let project = self.project(project_name).ok_or_else(|| {
            format!("No local checkout configured for Jira project {project_name}")
        })?;
        let workflow = self.workflow(project_name);
        if agent_starts && workflow.agent.is_empty() {
            return Err(format!(
                "No agent configured for Jira project {project_name}"
            ));
        }
        let detail = item_detail(item)
            .filter(|detail| !detail.base_branch.is_empty())
            .ok_or("issue details are not available yet; try again shortly")?;
        let branch = detail
            .existing_branch
            .clone()
            .unwrap_or_else(|| branch_name(&project.branch_template, key, &item.title));
        let base = &detail.base_branch;
        Ok(ProvisionPlan {
            source: WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: crate::worktree::expand_tilde_absolute_path(&project.path),
                remote: project.remote.clone(),
                // A private ref as base keeps the new branch from tracking the base branch.
                fetch_refspec: format!("+refs/heads/{base}:refs/herdr/base/{base}"),
                base_ref: format!("refs/herdr/base/{base}"),
                branch: branch.clone(),
                reuse_branch: true,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: Some(key.clone()),
            }),
            workspace_label: truncate_chars(&format!("{key} {}", item.title), MAX_LABEL_CHARS),
            agent_name_hint: key.to_ascii_lowercase(),
            brief: brief(item, &detail, agent_starts, &branch),
            layout: WorkspaceLayout {
                agent: workflow.agent.clone(),
                agent_args: workflow.agent_args.clone(),
                editor_command: workflow.editor_command.clone(),
                lazygit_command: workflow.lazygit_command.clone(),
                diff_command: String::new(),
                review_command: String::new(),
            },
            delete_branch: workflow.delete_branch,
        })
    }

    fn remove_on_resolved(&self, item: &WorkItem) -> bool {
        self.workflow(project_key(&item.external_id)).on_resolved == OnResolvedConfig::Remove
    }

    fn arrival_notice(&self, item: &SourceItem) -> (String, Option<String>) {
        (
            "Jira issue".into(),
            Some(format!("{} · {}", item.external_id, item.title)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn source() -> JiraSource {
        JiraSource::new(JiraWorkItemsConfig {
            site: "example.atlassian.net".into(),
            email: "me@example.test".into(),
            projects: vec![JiraProjectConfig {
                key: "TECH".into(),
                path: "/src/app".into(),
                remote: "origin".into(),
                base_branch: None,
                branch_template: "ar/{key}-{slug}".into(),
            }],
            ..JiraWorkItemsConfig::default()
        })
    }

    fn item(key: &str, detail: Option<&JiraDetail>) -> WorkItem {
        WorkItem {
            key: format!("jira:{key}"),
            source_id: "jira".into(),
            external_id: key.into(),
            title: "Add manual vehicle entry form".into(),
            context: format!("{key} · In Progress"),
            author: Some("Tony".into()),
            url: format!("https://example.atlassian.net/browse/{key}"),
            updated_at: "2026-09-24T15:51:59.000+0100".into(),
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
            waiting: false,
        }
    }

    fn detail() -> JiraDetail {
        JiraDetail {
            description: "Drivers enter make and model.".into(),
            issue_type: "Story".into(),
            priority: "Medium".into(),
            status: "In Progress".into(),
            labels: Vec::new(),
            comments: vec![JiraComment {
                author: "Tony".into(),
                body: "Design is in Figma.".into(),
            }],
            base_branch: "master".into(),
            existing_branch: None,
            existing_worktree: None,
        }
    }

    #[test]
    fn document_format_becomes_plain_text() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "Hello "},
                    {"type": "mention", "attrs": {"text": "@Andrea"}}
                ]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "one"}]}
                    ]}
                ]}
            ]
        });
        assert_eq!(adf_text(&adf), "Hello @Andrea\n- one");
    }

    #[test]
    fn search_results_become_items_keyed_by_issue() {
        let json = br#"{"issues":[{"key":"TECH-7","fields":{"summary":"Fix it",
            "updated":"2026-09-24T10:00:00.000+0100","status":{"name":"To Do"},
            "reporter":{"displayName":"Tony"}}}]}"#;
        assert_eq!(
            parse_search(json, |key| format!("https://x/browse/{key}")).unwrap(),
            (
                vec![SourceItem {
                    external_id: "TECH-7".into(),
                    title: "Fix it".into(),
                    context: "TECH-7 · To Do".into(),
                    author: Some("Tony".into()),
                    url: "https://x/browse/TECH-7".into(),
                    updated_at: "2026-09-24T10:00:00.000+0100".into(),
                }],
                None
            )
        );
    }

    #[test]
    fn mapped_project_works_on_a_templated_untracked_branch() {
        let source = source();
        let item = item("TECH-2031", Some(&detail()));
        assert_eq!(
            source.choices(&item).default_choice_id.as_deref(),
            Some("local")
        );
        let plan = source
            .provision_plan(&item, "local_agent", Path::new("/w"))
            .expect("plan");
        assert_eq!(
            plan.source,
            WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: PathBuf::from("/src/app"),
                remote: "origin".into(),
                fetch_refspec: "+refs/heads/master:refs/herdr/base/master".into(),
                base_ref: "refs/herdr/base/master".into(),
                branch: "ar/TECH-2031-add-manual-vehicle-entry-form".into(),
                reuse_branch: true,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: Some("TECH-2031".into()),
            })
        );
        assert!(plan
            .brief
            .contains("Jira issue TECH-2031: Add manual vehicle entry form"));
        assert!(plan.brief.contains("- Tony: Design is in Figma."));
        assert!(plan.brief.contains("Do not commit"));
    }

    #[test]
    fn existing_work_on_the_issue_is_offered_and_continued() {
        let source = source();
        let started = JiraDetail {
            existing_branch: Some("ar/tech-2031-started".into()),
            existing_worktree: Some("/w/app/ar-tech-2031-started".into()),
            ..detail()
        };
        let item = item("TECH-2031", Some(&started));
        let choices = source.choices(&item);
        assert_eq!(choices.choices[0].label, "Continue on ar/tech-2031-started");
        assert!(choices.choices[0]
            .description
            .as_deref()
            .is_some_and(|description| description.starts_with("Reopens your worktree")));
        let plan = source
            .provision_plan(&item, "local", Path::new("/w"))
            .expect("plan");
        let WorkspaceSource::Worktree(spec) = &plan.source else {
            panic!("worktree");
        };
        assert_eq!(spec.branch, "ar/tech-2031-started");
        assert!(plan.brief.contains("on the branch ar/tech-2031-started"));
    }

    #[test]
    fn unmapped_project_only_opens_in_jira() {
        let source = source();
        let choices = source.choices(&item("OPS-1", Some(&detail())));
        assert_eq!(choices.default_choice_id.as_deref(), Some("jira"));
        assert!(choices.choices[0].disabled_reason.is_some());
        assert!(source
            .provision_plan(&item("OPS-1", Some(&detail())), "local", Path::new("/w"))
            .is_err());
    }

    #[test]
    fn missing_site_or_email_is_reported_by_poll() {
        let source = JiraSource::new(JiraWorkItemsConfig::default());
        assert!(source.poll().unwrap_err().contains("work_items.jira.site"));
    }

    #[test]
    fn curl_config_values_are_quoted() {
        assert_eq!(curl_quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(branch_name("{key_lower}-{slug}", "TECH-1", "?!"), "tech-1");
    }
}
