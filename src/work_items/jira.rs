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
    CloseTicket, ItemChoices, PreparedItem, ProvisionPlan, SourceItem, StartReminder, TicketDetail,
    WorkItemSource, WorkspaceLayout, WorkspaceSource, WorktreeSpec, CLOSE_TICKET_CHOICE_ID,
    START_WORK_CHOICE_ID,
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
    /// The token owner's account id, to tell whether a ticket is assigned to you.
    me: Mutex<Option<String>>,
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
    status: Option<StatusField>,
    #[serde(default)]
    reporter: Option<Person>,
    #[serde(default)]
    assignee: Option<Person>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

/// Jira's status field, with the category used to tell finished statuses apart.
#[derive(Deserialize)]
struct StatusField {
    #[serde(default)]
    name: String,
    #[serde(rename = "statusCategory", default)]
    status_category: Option<StatusCategory>,
}

#[derive(Deserialize)]
struct StatusCategory {
    key: String,
}

#[derive(Clone, Deserialize)]
struct Person {
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(rename = "accountId", default)]
    account_id: String,
}

#[derive(Deserialize)]
struct TransitionsResponse {
    #[serde(default)]
    transitions: Vec<RestTransition>,
}

#[derive(Deserialize)]
struct RestTransition {
    id: String,
    #[serde(default)]
    to: Option<StatusField>,
}

#[derive(Deserialize)]
struct IssueResponse {
    fields: IssueFields,
}

#[derive(Deserialize)]
struct IssueFields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: Option<serde_json::Value>,
    #[serde(default)]
    issuetype: Option<Named>,
    #[serde(default)]
    priority: Option<Named>,
    #[serde(default)]
    status: Option<StatusField>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    comment: Option<CommentPage>,
    #[serde(default)]
    updated: String,
    #[serde(default)]
    assignee: Option<Person>,
    #[serde(default)]
    reporter: Option<Person>,
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
    /// Status category key: `new` (to do), `indeterminate` (in progress) or `done`. Empty
    /// for details stored before it was recorded, which never remind.
    #[serde(default)]
    pub status_category: String,
    /// Whether the issue is assigned to the token's owner; unknown for older details.
    #[serde(default)]
    pub assigned_to_me: Option<bool>,
    /// Transitions into an in-progress status, fetched while the issue is still to do.
    #[serde(default)]
    pub in_progress_transitions: Vec<JiraTransition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JiraTransition {
    pub id: String,
    /// Target status name, e.g. "In Progress".
    pub to: String,
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

const WRITE_SCOPE_HINT: &str = "Jira refused the change: the API token needs write:jira-work to \
     assign, start and close issues. After replacing it, run `herdr server live-handoff` from a \
     shell that has the new token so the server picks it up";

/// What starting work on an issue changes in Jira.
#[derive(Debug, PartialEq, Eq)]
struct StartPlan {
    assign: bool,
    transition: Option<JiraTransition>,
}

/// The changes that bring `detail` up to date with work you started, or why they cannot
/// be made from Herdr.
fn start_plan(detail: &JiraDetail) -> Result<StartPlan, String> {
    let transition = if detail.status_category == "new" {
        match detail.in_progress_transitions.as_slice() {
            [transition] => Some(transition.clone()),
            [] => {
                return Err("No transition leads to an in-progress status; move it in Jira".into())
            }
            // Workflows often add side routes such as "Blocked" in the same category; the
            // one named In Progress is what "move to In Progress" means.
            several => {
                let mut named = several
                    .iter()
                    .filter(|transition| transition.to.trim().eq_ignore_ascii_case("in progress"));
                match (named.next(), named.next()) {
                    (Some(transition), None) => Some(transition.clone()),
                    _ => {
                        return Err(format!(
                            "{} transitions lead to an in-progress status; move it in Jira",
                            several.len()
                        ))
                    }
                }
            }
        }
    } else {
        None
    };
    Ok(StartPlan {
        assign: detail.assigned_to_me == Some(false),
        transition,
    })
}

/// The transition that closes an issue: its only one into a done status, or among several
/// (e.g. "Done" and "Won't Do") the one named Done.
fn done_transition(transitions: &[JiraTransition]) -> Result<JiraTransition, String> {
    match transitions {
        [transition] => Ok(transition.clone()),
        [] => Err("No transition leads to a done status; move it in Jira".into()),
        several => {
            let mut named = several
                .iter()
                .filter(|transition| transition.to.trim().eq_ignore_ascii_case("done"));
            match (named.next(), named.next()) {
                (Some(transition), None) => Ok(transition.clone()),
                _ => Err(format!(
                    "{} transitions lead to a done status; move it in Jira",
                    several.len()
                )),
            }
        }
    }
}

fn item_detail(item: &WorkItem) -> Option<JiraDetail> {
    item.detail
        .clone()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// The reminder for an issue you work on while Jira lags behind: not assigned to you,
/// or still in a to-do status.
fn start_reminder_for(detail: &JiraDetail) -> Option<StartReminder> {
    let unassigned = detail.assigned_to_me == Some(false);
    let to_do = detail.status_category == "new";
    let status = &detail.status;
    let (message, label) = match (unassigned, to_do) {
        (false, false) => return None,
        (true, true) => (
            format!("You're working on this, but it isn't assigned to you and is still {status}"),
            "Assign to me and move to In Progress",
        ),
        (true, false) => (
            "You're working on this, but it isn't assigned to you".to_string(),
            "Assign to me",
        ),
        (false, true) => (
            format!("You're working on this, but it is still {status}"),
            "Move to In Progress",
        ),
    };
    let disabled_reason = start_plan(detail).err();
    Some(StartReminder {
        message,
        choice: WorkItemChoiceInfo {
            choice_id: START_WORK_CHOICE_ID.into(),
            label: label.into(),
            description: Some("Updates the issue in Jira".into()),
            action: WorkItemChoiceAction::Perform,
            disabled_reason,
            confirm: None,
        },
    })
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
            me: Mutex::new(None),
        }
    }

    /// The token owner's account id, asked once.
    fn my_account_id(&self) -> Result<String, String> {
        if let Some(me) = self.me.lock().ok().and_then(|me| me.clone()) {
            return Ok(me);
        }
        let me = serde_json::from_slice::<Person>(&self.api("GET", "myself", None)?)
            .map_err(|err| format!("unexpected Jira response: {err}"))?
            .account_id;
        if me.is_empty() {
            return Err("Jira did not say who the token belongs to".into());
        }
        if let Ok(mut cached) = self.me.lock() {
            *cached = Some(me.clone());
        }
        Ok(me)
    }

    /// Transitions of `key` into a status of `category`: `indeterminate` (in progress) or
    /// `done`.
    fn transitions_into(&self, key: &str, category: &str) -> Result<Vec<JiraTransition>, String> {
        let response: TransitionsResponse =
            serde_json::from_slice(&self.api("GET", &format!("issue/{key}/transitions"), None)?)
                .map_err(|err| format!("unexpected Jira response: {err}"))?;
        Ok(response
            .transitions
            .into_iter()
            .filter_map(|transition| {
                let to = transition.to?;
                to.status_category
                    .as_ref()
                    .is_some_and(|status_category| status_category.key == category)
                    .then_some(JiraTransition {
                        id: transition.id,
                        to: to.name,
                    })
            })
            .collect())
    }

    /// Moves the issue to its done status, once the work for it is merged.
    fn close_issue(&self, item: &WorkItem) -> Result<String, String> {
        let key = &item.external_id;
        let transition = done_transition(&self.transitions_into(key, "done")?)?;
        self.write(
            "POST",
            &format!("issue/{key}/transitions"),
            &serde_json::json!({ "transition": { "id": transition.id } }).to_string(),
        )?;
        Ok(format!("{key} moved to {}", transition.to))
    }

    /// A change to an issue.
    fn write(&self, method: &str, path: &str, body: &str) -> Result<(), String> {
        let (status, response) = self.api_with_status(method, path, Some(body))?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        if matches!(status, 401 | 403) {
            return Err(WRITE_SCOPE_HINT.into());
        }
        Err(http_error(status, &response))
    }

    /// Assigns the issue to you and moves it to in progress, as far as each is needed.
    fn start_work(&self, item: &WorkItem) -> Result<String, String> {
        let detail = item_detail(item).ok_or("the issue details are not loaded yet")?;
        let plan = start_plan(&detail)?;
        let key = &item.external_id;
        if plan.assign {
            let me = self.my_account_id()?;
            self.write(
                "PUT",
                &format!("issue/{key}/assignee"),
                &serde_json::json!({ "accountId": me }).to_string(),
            )?;
        }
        if let Some(transition) = &plan.transition {
            self.write(
                "POST",
                &format!("issue/{key}/transitions"),
                &serde_json::json!({ "transition": { "id": transition.id } }).to_string(),
            )?;
        }
        Ok(match (plan.assign, &plan.transition) {
            (true, Some(transition)) => format!("{key} is yours and {}", transition.to),
            (true, None) => format!("{key} is assigned to you"),
            (false, Some(transition)) => format!("{key} moved to {}", transition.to),
            (false, None) => format!("{key} is already up to date"),
        })
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
                    "{} is not set in the Herdr server's environment (work_items.jira.token_env); \
                     set it, then run `herdr server live-handoff` from that shell",
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
            403 => "the Jira API token lacks permission (read:jira-user, read:jira-work; \
                    write:jira-work to assign and start issues)"
                .into(),
            status => format!("Jira returned HTTP {status}"),
        })
    }

    /// A REST call under `/rest/api/3`, exposing the status code so callers can tell a
    /// missing resource apart from other failures.
    fn api_with_status(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, Vec<u8>), String> {
        let base = self.api_base()?;
        let (status, response) = self.http(method, &format!("{base}/rest/api/3/{path}"), body)?;
        if matches!(status, 401 | 403) {
            // The token may have been rotated; find the base again next time.
            if let Ok(mut cached) = self.api_base.lock() {
                *cached = None;
            }
        }
        Ok((status, response))
    }

    /// A REST call under `/rest/api/3`.
    fn api(&self, method: &str, path: &str, body: Option<&str>) -> Result<Vec<u8>, String> {
        let (status, response) = self.api_with_status(method, path, body)?;
        if (200..300).contains(&status) {
            return Ok(response);
        }
        Err(http_error(status, &response))
    }

    /// The `fields` of one issue; `None` when `key` does not exist.
    fn issue(&self, key: &str, fields: &str) -> Result<Option<IssueFields>, String> {
        let (status, body) =
            self.api_with_status("GET", &format!("issue/{key}?fields={fields}"), None)?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(http_error(status, &body));
        }
        let issue: IssueResponse = serde_json::from_slice(&body)
            .map_err(|err| format!("unexpected Jira response: {err}"))?;
        Ok(Some(issue.fields))
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

/// The ticket's status and who it is assigned to, shown under its title.
fn tracker_state(status: &str, assignee: Option<&str>) -> String {
    let assignee = assignee.unwrap_or("unassigned");
    if status.is_empty() {
        assignee.to_string()
    } else {
        format!("{status} · {assignee}")
    }
}

/// The issue key a title starts with, bare or in brackets: `TECH-12` for "TECH-12 Fix",
/// "[TECH-12] Fix" or "TECH-12: Fix". Project keys start with an uppercase letter and hold
/// uppercase letters, digits and underscores.
fn title_issue_key(title: &str) -> Option<&str> {
    let rest = title.trim_start();
    let rest = rest.strip_prefix(['[', '(']).unwrap_or(rest);
    if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let project_len = rest
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    let number = rest[project_len..].strip_prefix('-')?;
    let digits = number
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(number.len());
    if digits == 0 || number[digits..].starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(&rest[..project_len + 1 + digits])
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
            let assignee = issue
                .fields
                .assignee
                .map(|person| person.display_name)
                .filter(|name| !name.is_empty());
            SourceItem {
                context: issue.key.clone(),
                url: browse_url(&issue.key),
                external_id: issue.key,
                title: issue.fields.summary,
                author: issue
                    .fields
                    .reporter
                    .map(|person| person.display_name)
                    .filter(|name| !name.is_empty()),
                updated_at: issue.fields.updated,
                tracker_state: Some(tracker_state(&status, assignee.as_deref())),
            }
        })
        .collect();
    Ok((items, next))
}

/// Search results as tickets, independent of the continuous poll.
fn parse_ticket_search(
    bytes: &[u8],
    browse_url: impl Fn(&str) -> String,
) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String> {
    let response: SearchResponse =
        serde_json::from_slice(bytes).map_err(|err| format!("unexpected Jira response: {err}"))?;
    Ok(response
        .issues
        .into_iter()
        .map(|issue| {
            let status = issue
                .fields
                .status
                .as_ref()
                .map(|status| status.name.clone())
                .unwrap_or_default();
            let done = issue
                .fields
                .status
                .as_ref()
                .and_then(|status| status.status_category.as_ref())
                .is_some_and(|category| category.key.eq_ignore_ascii_case("done"));
            crate::api::schema::WorkItemTicketInfo {
                key: issue.key.clone(),
                title: issue.fields.summary,
                status,
                done,
                assignee: issue
                    .fields
                    .assignee
                    .map(|person| person.display_name)
                    .filter(|name| !name.is_empty()),
                updated_at: issue.fields.updated,
                url: browse_url(&issue.key),
            }
        })
        .collect())
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

/// Brief for the shared "Pick next" discovery workspace: read-only investigation of the
/// tracker, ending in a recommendation the user confirms before anything is added or chosen.
fn pick_next_brief(projects: &[JiraProjectConfig], context: &str) -> String {
    let clones = if projects.is_empty() {
        "(no projects are mapped to a local clone; use the Jira issue text for detail)".to_string()
    } else {
        projects
            .iter()
            .map(|project| format!("- {} ({})", project.path, project.key))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let keys: Vec<&str> = projects
        .iter()
        .map(|project| project.key.as_str())
        .collect();
    let query = if keys.is_empty() {
        "assignee = currentUser() AND sprint in openSprints() ORDER BY status, updated DESC"
            .to_string()
    } else {
        format!(
            "project in ({}) AND sprint in openSprints() ORDER BY status, assignee",
            keys.join(", ")
        )
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
         - `herdr work-item search jira \"<JQL>\"`, e.g.:\n\
         \x20 herdr work-item search jira \"{query}\"\n\
         - `herdr work-item show jira <KEY>` — one issue's full detail.\n\
         - `herdr work-item list` — the current inbox, with each item's id and choice ids.\n\
         - `herdr work-item add jira <KEY>` — brings an issue into the inbox (only after I \
         confirm, see below).\n\
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
         Only once I confirm a pick in this chat: run `herdr work-item add jira <key>`, then \
         `herdr work-item choose <item-id> <choice-id>` defaulting to the local choice (an \
         agent-led one only if I ask); take ids from `herdr work-item list`, never guess them. \
         Do not assign the issue to me or move it — I'll be reminded about that separately.",
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
                "fields": ["summary", "status", "updated", "reporter", "assignee"],
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
        let fields = "description,issuetype,priority,status,labels,comment,assignee";
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
                    done: false,
                }
            }
        };
        let done = fields
            .status
            .as_ref()
            .and_then(|status| status.status_category.as_ref())
            .is_some_and(|category| category.key.eq_ignore_ascii_case("done"));
        let status_category = fields
            .status
            .as_ref()
            .and_then(|status| status.status_category.as_ref())
            .map(|category| category.key.clone())
            .unwrap_or_default();
        // Unknown when Jira will not say who you are: then no reminder, rather than a wrong one.
        let assigned_to_me = self.my_account_id().ok().map(|me| {
            fields
                .assignee
                .as_ref()
                .is_some_and(|assignee| assignee.account_id == me)
        });
        let mut error = None;
        let project = self.project(project_key(&item.external_id));
        let base_branch = match project {
            Some(project) => self.base_branch(project).unwrap_or_else(|err| {
                error = Some(err);
                String::new()
            }),
            None => String::new(),
        };
        // Only an issue still to do needs a transition to start it.
        let in_progress_transitions = if status_category == "new" {
            self.transitions_into(&item.external_id, "indeterminate")
                .unwrap_or_else(|err| {
                    error.get_or_insert(format!("transitions unavailable: {err}"));
                    Vec::new()
                })
        } else {
            Vec::new()
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
            status: fields
                .status
                .as_ref()
                .map(|status| status.name.clone())
                .unwrap_or_default(),
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
            status_category,
            assigned_to_me,
            in_progress_transitions,
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
            done,
        }
    }

    fn choices(&self, item: &WorkItem) -> ItemChoices {
        if item.is_pick_next {
            return ItemChoices {
                choices: Vec::new(),
                default_choice_id: None,
            };
        }
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
                tabs: workflow.tabs.clone(),
                diff_command: String::new(),
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

    fn tracker_need(&self, item: &WorkItem) -> Option<super::attention::Need> {
        let reason = item.tracker_state.as_deref().unwrap_or("New ticket");
        Some(super::attention::Need::new(
            crate::api::schema::AttentionKind::New,
            reason,
        ))
    }

    fn search(&self, query: &str) -> Result<Vec<crate::api::schema::WorkItemTicketInfo>, String> {
        if let Some(error) = &self.build_error {
            return Err(error.clone());
        }
        let body = serde_json::json!({
            "jql": query,
            "maxResults": PAGE_SIZE,
            "fields": ["summary", "status", "updated", "assignee"],
        });
        // Single page: an on-demand lookup, not the continuous poll.
        let response = self.api("POST", "search/jql", Some(&body.to_string()))?;
        parse_ticket_search(&response, |key| self.browse_url(key))
    }

    fn fetch(&self, key: &str) -> Result<Option<TicketDetail>, String> {
        if let Some(error) = &self.build_error {
            return Err(error.clone());
        }
        let fields = "summary,description,issuetype,priority,status,labels,comment,updated,\
                      assignee,reporter";
        let Some(fields) = self.issue(key, fields)? else {
            return Ok(None);
        };
        let status_name = fields
            .status
            .as_ref()
            .map(|status| status.name.clone())
            .unwrap_or_default();
        let done = fields
            .status
            .as_ref()
            .and_then(|status| status.status_category.as_ref())
            .is_some_and(|category| category.key.eq_ignore_ascii_case("done"));
        let assignee = fields
            .assignee
            .clone()
            .map(|person| person.display_name)
            .filter(|name| !name.is_empty());
        let description = fields
            .description
            .as_ref()
            .map(adf_text)
            .unwrap_or_default();
        let comments = fields
            .comment
            .map(|page| page.comments)
            .unwrap_or_default()
            .into_iter()
            .map(|comment| crate::api::schema::WorkItemTicketComment {
                author: comment
                    .author
                    .map(|person| person.display_name)
                    .unwrap_or_else(|| "unknown".into()),
                body: comment.body.as_ref().map(adf_text).unwrap_or_default(),
            })
            .collect();
        let tracker_state = tracker_state(&status_name, assignee.as_deref());
        let ticket = crate::api::schema::WorkItemTicketInfo {
            key: key.to_string(),
            title: fields.summary.clone(),
            status: status_name,
            done,
            assignee: assignee.clone(),
            updated_at: fields.updated.clone(),
            url: self.browse_url(key),
        };
        let source_item = SourceItem {
            external_id: key.to_string(),
            title: fields.summary,
            context: key.to_string(),
            author: fields
                .reporter
                .map(|person| person.display_name)
                .filter(|name| !name.is_empty()),
            url: self.browse_url(key),
            updated_at: fields.updated,
            tracker_state: Some(tracker_state),
        };
        Ok(Some(TicketDetail {
            ticket,
            description,
            comments,
            source_item,
        }))
    }

    fn ticket_key_in_title(&self, title: &str) -> Option<String> {
        title_issue_key(title).map(str::to_string)
    }

    fn linked_ticket(
        &self,
        key: &str,
    ) -> Result<Option<crate::api::schema::WorkItemLinkedTicketInfo>, String> {
        if let Some(error) = &self.build_error {
            return Err(error.clone());
        }
        let Some(fields) = self.issue(key, "status,assignee")? else {
            return Ok(None);
        };
        let status = fields.status.map(|status| status.name).unwrap_or_default();
        let assignee = fields
            .assignee
            .map(|person| person.display_name)
            .filter(|name| !name.is_empty());
        Ok(Some(crate::api::schema::WorkItemLinkedTicketInfo {
            source_id: SOURCE_ID.into(),
            key: key.to_string(),
            url: self.browse_url(key),
            tracker_state: tracker_state(&status, assignee.as_deref()),
        }))
    }

    fn start_reminder(&self, item: &WorkItem) -> Option<StartReminder> {
        start_reminder_for(&item_detail(item)?)
    }

    fn work_branch(&self, item: &WorkItem) -> Option<(std::path::PathBuf, String)> {
        let branch = item_detail(item)?.existing_branch?;
        let project = self.project(project_key(&item.external_id))?;
        Some((
            crate::worktree::expand_tilde_absolute_path(&project.path),
            branch,
        ))
    }

    fn close_ticket(
        &self,
        item: &WorkItem,
        pull_request: &crate::api::schema::WorkItemPullRequestInfo,
    ) -> Option<CloseTicket> {
        let detail = item_detail(item);
        if detail
            .as_ref()
            .is_some_and(|detail| detail.status_category == "done")
        {
            return None;
        }
        let key = &item.external_id;
        let status = detail
            .map(|detail| detail.status)
            .filter(|status| !status.is_empty())
            .unwrap_or_else(|| "open".into());
        Some(CloseTicket {
            reason: format!(
                "{}#{} merged; {key} is still {status}",
                pull_request.repo, pull_request.number
            ),
            choice: WorkItemChoiceInfo {
                choice_id: CLOSE_TICKET_CHOICE_ID.into(),
                label: format!("Move {key} to Done"),
                description: Some(format!(
                    "{}#{} is merged; updates the issue in Jira",
                    pull_request.repo, pull_request.number
                )),
                action: WorkItemChoiceAction::Perform,
                disabled_reason: None,
                confirm: None,
            },
        })
    }

    fn perform(&self, item: &WorkItem, choice_id: &str) -> Result<String, String> {
        match choice_id {
            START_WORK_CHOICE_ID => self.start_work(item),
            CLOSE_TICKET_CHOICE_ID => self.close_issue(item),
            _ => Err(format!("choice {choice_id} cannot be carried out here")),
        }
    }

    fn pick_next_plan(
        &self,
        context: &str,
        worktree_directory: &Path,
    ) -> Result<ProvisionPlan, String> {
        if let Some(error) = &self.build_error {
            return Err(error.clone());
        }
        // The agent you work issues with; the first mapped project picks the block.
        let first_project = self
            .config
            .projects
            .first()
            .map_or("", |project| project.key.as_str());
        let config = self.workflow(first_project);
        if config.agent.is_empty() {
            return Err(
                "No agent configured for Jira; set work_items.jira.issues.<block>.agent".into(),
            );
        }
        let directory =
            crate::worktree::default_checkout_path(worktree_directory, "pick-next", "jira");
        Ok(ProvisionPlan {
            source: WorkspaceSource::Scratch(directory),
            workspace_label: format!("Pick next \u{b7} {}", self.label()),
            agent_name_hint: "pick-next-jira".into(),
            brief: pick_next_brief(&self.config.projects, context),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn only_a_key_at_the_start_of_a_title_names_an_issue() {
        for (title, key) in [
            ("TECH-12 Fix login", Some("TECH-12")),
            ("[TECH-12] Fix login", Some("TECH-12")),
            ("  (OPS2-7): Fix login", Some("OPS2-7")),
            ("TECH_OPS-3", Some("TECH_OPS-3")),
            ("Fix login for TECH-12", None),
            ("tech-12 Fix login", None),
            ("TECH- Fix login", None),
            ("TECH-12a Fix login", None),
            ("2FA-1 Fix login", None),
        ] {
            assert_eq!(title_issue_key(title), key, "{title}");
        }
    }

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
            context: key.into(),
            author: Some("Tony".into()),
            url: format!("https://example.atlassian.net/browse/{key}"),
            updated_at: "2026-09-24T15:51:59.000+0100".into(),
            tracker_state: Some("In Progress · Tony".into()),
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
            status_category: "indeterminate".into(),
            assigned_to_me: Some(true),
            in_progress_transitions: Vec::new(),
        }
    }

    fn to_do(assigned_to_me: bool, transitions: &[&str]) -> JiraDetail {
        JiraDetail {
            status: "To Do".into(),
            status_category: "new".into(),
            assigned_to_me: Some(assigned_to_me),
            in_progress_transitions: transitions
                .iter()
                .enumerate()
                .map(|(index, to)| JiraTransition {
                    id: (index + 11).to_string(),
                    to: (*to).into(),
                })
                .collect(),
            ..detail()
        }
    }

    #[test]
    fn merged_work_offers_to_close_an_open_issue_but_not_a_done_one() {
        let merged = crate::api::schema::WorkItemPullRequestInfo {
            source_id: "github".into(),
            repo: "o/r".into(),
            number: 12,
            url: "https://example.test/o/r/pull/12".into(),
            is_draft: false,
            status: "merged".into(),
        };
        let close = source()
            .close_ticket(&item("TECH-7", Some(&detail())), &merged)
            .expect("an open issue can be closed");
        assert_eq!(close.reason, "o/r#12 merged; TECH-7 is still In Progress");
        assert_eq!(close.choice.label, "Move TECH-7 to Done");
        assert_eq!(close.choice.choice_id, CLOSE_TICKET_CHOICE_ID);

        let done = JiraDetail {
            status: "Done".into(),
            status_category: "done".into(),
            ..detail()
        };
        assert_eq!(
            source().close_ticket(&item("TECH-7", Some(&done)), &merged),
            None
        );
    }

    #[test]
    fn work_branch_is_the_issues_existing_branch_in_its_project_clone() {
        let on_branch = JiraDetail {
            existing_branch: Some("ar/TECH-7-form".into()),
            ..detail()
        };
        assert_eq!(
            source().work_branch(&item("TECH-7", Some(&on_branch))),
            Some(("/src/app".into(), "ar/TECH-7-form".to_string()))
        );
        assert_eq!(source().work_branch(&item("TECH-7", Some(&detail()))), None);
        // A project without a local clone has no branch to look up.
        assert_eq!(source().work_branch(&item("OPS-3", Some(&on_branch))), None);
    }

    #[test]
    fn closing_picks_the_one_done_transition_or_the_one_named_done() {
        let transitions = |names: &[&str]| -> Vec<JiraTransition> {
            names
                .iter()
                .enumerate()
                .map(|(index, to)| JiraTransition {
                    id: (index + 31).to_string(),
                    to: (*to).into(),
                })
                .collect()
        };
        assert_eq!(
            done_transition(&transitions(&["Released"])).map(|transition| transition.to),
            Ok("Released".to_string())
        );
        assert_eq!(
            done_transition(&transitions(&["Won't Do", "Done"])).map(|transition| transition.id),
            Ok("32".to_string())
        );
        assert!(done_transition(&transitions(&["Won't Do", "Duplicate"])).is_err());
        assert!(done_transition(&[]).is_err());
    }

    #[test]
    fn up_to_date_issue_has_no_start_reminder() {
        assert_eq!(start_reminder_for(&detail()), None);
        // Details stored before assignment was recorded never remind.
        let unknown = JiraDetail {
            assigned_to_me: None,
            ..detail()
        };
        assert_eq!(start_reminder_for(&unknown), None);
    }

    #[test]
    fn unassigned_to_do_issue_offers_assigning_and_starting_in_one_step() {
        let reminder = start_reminder_for(&to_do(false, &["In Progress"])).expect("reminder");
        assert!(reminder.message.contains("isn't assigned to you"));
        assert!(reminder.message.contains("To Do"));
        assert_eq!(
            reminder.choice.label,
            "Assign to me and move to In Progress"
        );
        assert_eq!(reminder.choice.disabled_reason, None);
        assert_eq!(
            start_plan(&to_do(false, &["In Progress"])),
            Ok(StartPlan {
                assign: true,
                transition: Some(JiraTransition {
                    id: "11".into(),
                    to: "In Progress".into(),
                }),
            })
        );
    }

    #[test]
    fn start_reminder_label_names_only_the_missing_step() {
        let move_only = start_reminder_for(&to_do(true, &["In Progress"])).expect("reminder");
        assert_eq!(move_only.choice.label, "Move to In Progress");
        let assign_only = JiraDetail {
            assigned_to_me: Some(false),
            ..detail()
        };
        let reminder = start_reminder_for(&assign_only).expect("reminder");
        assert_eq!(reminder.choice.label, "Assign to me");
        assert_eq!(
            start_plan(&assign_only),
            Ok(StartPlan {
                assign: true,
                transition: None,
            })
        );
    }

    #[test]
    fn start_fix_is_disabled_without_a_single_in_progress_transition() {
        for transitions in [&[][..], &["Doing", "In Review"][..]] {
            let reminder = start_reminder_for(&to_do(true, transitions)).expect("still reminds");
            assert!(reminder.choice.disabled_reason.is_some(), "{transitions:?}");
        }
    }

    #[test]
    fn start_fix_takes_the_in_progress_transition_among_side_routes() {
        let detail = to_do(true, &["BLOCKED / ON HOLD", "In Progress"]);
        assert_eq!(
            start_plan(&detail),
            Ok(StartPlan {
                assign: false,
                transition: Some(JiraTransition {
                    id: "12".into(),
                    to: "In Progress".into(),
                }),
            })
        );
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
            "reporter":{"displayName":"Tony"}}},
            {"key":"TECH-8","fields":{"summary":"Ship it","updated":"2026-09-24T11:00:00.000+0100",
            "status":{"name":"In Progress"},"assignee":{"displayName":"Ada"}}}]}"#;
        assert_eq!(
            parse_search(json, |key| format!("https://x/browse/{key}")).unwrap(),
            (
                vec![
                    SourceItem {
                        external_id: "TECH-7".into(),
                        title: "Fix it".into(),
                        context: "TECH-7".into(),
                        author: Some("Tony".into()),
                        url: "https://x/browse/TECH-7".into(),
                        updated_at: "2026-09-24T10:00:00.000+0100".into(),
                        tracker_state: Some("To Do · unassigned".into()),
                    },
                    SourceItem {
                        external_id: "TECH-8".into(),
                        title: "Ship it".into(),
                        context: "TECH-8".into(),
                        author: None,
                        url: "https://x/browse/TECH-8".into(),
                        updated_at: "2026-09-24T11:00:00.000+0100".into(),
                        tracker_state: Some("In Progress · Ada".into()),
                    },
                ],
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
