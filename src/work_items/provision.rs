//! Local provisioning of a work item: progress model and background workers.
//!
//! Progress transitions are pure; the workers block and must only run on
//! background threads. Worktrees themselves are created by the app through
//! Herdr's `worktree.create`, so worktree plugin hooks run for them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::api::schema::{
    WorkItemProvisioningInfo, WorkItemStep, WorkItemStepInfo, WorkItemStepStatus,
};

use super::process::{failure_detail, run_with_timeout};
use super::source::{DownloadSpec, ProvisionPlan, WorkspaceSource, WorktreeSpec};

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Review branches `<branch>-2` … `<branch>-N` are tried when `<branch>` is kept.
const MAX_BRANCH_SUFFIX: u32 = 9;

/// Retry state for starting the agent or delivering its brief.
#[derive(Debug)]
pub(crate) struct AgentAttempt {
    pub started: Instant,
    pub next_attempt: Instant,
    /// Response channel of an in-flight `agent.prompt`.
    pub pending: Option<std::sync::mpsc::Receiver<String>>,
    /// The agent was last seen asking the user something.
    pub blocked: bool,
}

/// An in-flight deferred API request (e.g. `worktree.create`).
#[derive(Debug)]
pub(crate) struct PendingResponse {
    pub next_check: Instant,
    pub response: std::sync::mpsc::Receiver<String>,
}

/// A delivered brief waiting for the agent to show that it started working.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BriefConfirmation {
    pub sent: Instant,
    pub next_check: Instant,
}

#[derive(Debug)]
pub(crate) struct ProvisionJob {
    pub job_id: u64,
    pub key: String,
    pub plan: ProvisionPlan,
    pub workspace_id: Option<String>,
    pub agent_pane_id: Option<String>,
    pub agent_name: Option<String>,
    /// Branch the worktree was created on, once known.
    pub branch: Option<String>,
    /// Waiting for Herdr to create the worktree.
    pub worktree: Option<PendingResponse>,
    /// Waiting for the shell in the agent pane to accept `agent.start`.
    pub agent_start: Option<AgentAttempt>,
    /// Waiting for the agent to accept its brief.
    pub brief: Option<AgentAttempt>,
    /// Waiting for the agent to start working on its brief.
    pub brief_confirmation: Option<BriefConfirmation>,
}

impl ProvisionJob {
    pub(crate) fn next_attempt(&self) -> Option<Instant> {
        [
            self.worktree.as_ref().map(|pending| pending.next_check),
            self.agent_start
                .as_ref()
                .map(|attempt| attempt.next_attempt),
            self.brief.as_ref().map(|attempt| attempt.next_attempt),
            self.brief_confirmation
                .as_ref()
                .map(|confirmation| confirmation.next_check),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// Outcome of preparing the workspace source on a background thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceReady {
    /// Create a worktree on `branch`; a branch that does not exist yet starts at `base`.
    CreateWorktree { branch: String, base: String },
    /// `branch` is already checked out at `path`; reopen that worktree.
    ExistingWorktree { path: PathBuf, branch: String },
    /// The download workspace's file is in place.
    Downloaded,
}

fn step(
    step: WorkItemStep,
    label: &str,
    status: WorkItemStepStatus,
    detail: Option<&str>,
) -> WorkItemStepInfo {
    WorkItemStepInfo {
        step,
        label: label.to_string(),
        status,
        detail: detail.map(str::to_string),
    }
}

/// Progress right after the user chose a provisioning choice.
pub(crate) fn initial_progress(plan: &ProvisionPlan) -> WorkItemProvisioningInfo {
    use WorkItemStepStatus::{Pending, Running, Skipped};
    let first_label = match &plan.source {
        WorkspaceSource::Worktree(_) => "Worktree created",
        WorkspaceSource::Download(_) => "Diff downloaded",
    };
    let agent_brief = if plan.layout.agent.is_empty() {
        step(
            WorkItemStep::AgentBrief,
            "Agent briefed",
            Skipped,
            Some("no agent configured"),
        )
    } else {
        step(WorkItemStep::AgentBrief, "Agent briefed", Pending, None)
    };
    let mut progress = WorkItemProvisioningInfo {
        steps: vec![
            step(WorkItemStep::Checkout, first_label, Running, None),
            agent_brief,
        ],
        finished: false,
    };
    refresh_finished(&mut progress);
    progress
}

pub(crate) fn status(
    progress: &WorkItemProvisioningInfo,
    which: WorkItemStep,
) -> Option<WorkItemStepStatus> {
    progress
        .steps
        .iter()
        .find(|info| info.step == which)
        .map(|info| info.status)
}

/// Sets one step and recomputes `finished`.
pub(crate) fn set_step(
    progress: &mut WorkItemProvisioningInfo,
    which: WorkItemStep,
    status: WorkItemStepStatus,
    detail: Option<String>,
) {
    if let Some(info) = progress.steps.iter_mut().find(|info| info.step == which) {
        info.status = status;
        info.detail = detail;
    }
    refresh_finished(progress);
}

/// Marks every step that has not finished with `status` and `detail`.
pub(crate) fn end_unfinished(
    progress: &mut WorkItemProvisioningInfo,
    status: WorkItemStepStatus,
    detail: &str,
) {
    for info in &mut progress.steps {
        if !is_terminal(info.status) {
            info.status = status;
            info.detail = Some(detail.to_string());
        }
    }
    refresh_finished(progress);
}

/// The workspace could not be prepared: record it and skip everything that depends on it.
pub(crate) fn fail_checkout(progress: &mut WorkItemProvisioningInfo, detail: String) {
    set_step(
        progress,
        WorkItemStep::Checkout,
        WorkItemStepStatus::Failed,
        Some(detail),
    );
    end_unfinished(progress, WorkItemStepStatus::Skipped, "no workspace");
}

pub(crate) fn has_failure(progress: &WorkItemProvisioningInfo) -> bool {
    progress
        .steps
        .iter()
        .any(|info| info.status == WorkItemStepStatus::Failed)
}

fn is_terminal(status: WorkItemStepStatus) -> bool {
    matches!(
        status,
        WorkItemStepStatus::Done | WorkItemStepStatus::Skipped | WorkItemStepStatus::Failed
    )
}

fn refresh_finished(progress: &mut WorkItemProvisioningInfo) {
    progress.finished = progress.steps.iter().all(|info| is_terminal(info.status));
}

fn git_output(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    run_with_timeout(command, timeout).map_err(|err| err.to_string())
}

fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<(), String> {
    let output = git_output(cwd, args, timeout)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure_detail(&output))
    }
}

fn branch_exists(repo: &Path, branch: &str) -> Result<bool, String> {
    let output = git_output(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        GIT_TIMEOUT,
    )?;
    Ok(output.status.success())
}

/// Worktrees of `repo` as (path, branch) pairs; detached ones have no branch.
fn worktrees(repo: &Path) -> Result<Vec<(PathBuf, Option<String>)>, String> {
    let output = git_output(repo, &["worktree", "list", "--porcelain"], GIT_TIMEOUT)?;
    if !output.status.success() {
        return Err(failure_detail(&output));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push((PathBuf::from(path), None));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(last) = worktrees.last_mut() {
                last.1 = Some(branch.to_string());
            }
        }
    }
    Ok(worktrees)
}

/// Path of the worktree that has `branch` checked out, if any.
fn worktree_for_branch(repo: &Path, branch: &str) -> Result<Option<PathBuf>, String> {
    Ok(worktrees(repo)?
        .into_iter()
        .find(|(_, checked_out)| checked_out.as_deref() == Some(branch))
        .map(|(path, _)| path))
}

/// Whether `branch` names the issue `key`: the key appears, case-insensitively, neither
/// inside a longer word nor followed by more digits (`tech-20` does not match `tech-2096`).
pub(crate) fn branch_matches_key(branch: &str, key: &str) -> bool {
    let branch = branch.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    if key.is_empty() {
        return false;
    }
    branch.match_indices(&key).any(|(start, _)| {
        let before = branch[..start].chars().next_back();
        let after = branch[start + key.len()..].chars().next();
        before.is_none_or(|character| !character.is_ascii_alphanumeric())
            && after.is_none_or(|character| !character.is_ascii_digit())
    })
}

/// Existing work for an issue in `repo`, most specific first: a worktree whose branch names
/// `key`; a local branch that does; else a branch holding the newest commit whose message
/// mentions `key` and is not on `base_rev` (branches named after the change rather than the
/// issue), other than `base_branch`. Returns the branch and, when checked out, its worktree.
pub(crate) fn find_existing_work(
    repo: &Path,
    key: &str,
    base_branch: &str,
    base_rev: &str,
) -> Result<Option<(String, Option<PathBuf>)>, String> {
    let worktrees = worktrees(repo)?;
    let checkout = |branch: &str| {
        worktrees
            .iter()
            .find(|(_, checked_out)| checked_out.as_deref() == Some(branch))
            .map(|(path, _)| path.clone())
    };
    for (path, branch) in &worktrees {
        if let Some(branch) = branch
            .as_ref()
            .filter(|branch| branch_matches_key(branch, key))
        {
            return Ok(Some((branch.clone(), Some(path.clone()))));
        }
    }
    let lines = |args: &[&str]| -> Result<Vec<String>, String> {
        let output = git_output(repo, args, GIT_TIMEOUT)?;
        if !output.status.success() {
            return Err(failure_detail(&output));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    };
    let branches = lines(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    if let Some(branch) = branches
        .iter()
        .find(|branch| branch_matches_key(branch, key))
    {
        return Ok(Some((branch.clone(), checkout(branch))));
    }
    let grep = format!("--grep={key}");
    let mut log = vec![
        "log",
        "--branches",
        "--fixed-strings",
        "--regexp-ignore-case",
        grep.as_str(),
        "--max-count=1",
        "--format=%H",
    ];
    // Commits that reached the base are in every branch started since; only unmerged work
    // says which branch the issue is on.
    let base_known = !base_rev.is_empty()
        && git_output(
            repo,
            &["rev-parse", "--verify", "--quiet", base_rev],
            GIT_TIMEOUT,
        )?
        .status
        .success();
    if base_known {
        log.extend(["--not", base_rev]);
    }
    let Some(commit) = lines(&log)?.into_iter().next() else {
        return Ok(None);
    };
    let containing: Vec<String> =
        lines(&["branch", "--contains", &commit, "--format=%(refname:short)"])?
            .into_iter()
            .filter(|branch| branch != base_branch)
            .collect();
    // Several branches can hold the commit (a branch started from another); the one that is
    // checked out is the one being worked on.
    let branch = containing
        .iter()
        .find(|branch| checkout(branch).is_some())
        .or(containing.first());
    Ok(branch.map(|branch| (branch.clone(), checkout(branch))))
}

/// Branch name of a `refs/herdr/base/<branch>` base, for excluding it from adoption.
fn base_branch_name(base_ref: &str) -> &str {
    base_ref
        .strip_prefix("refs/herdr/base/")
        .unwrap_or(base_ref)
}

/// Fetches the change and decides how Herdr should create or reopen its worktree.
fn prepare_worktree(spec: &WorktreeSpec) -> Result<SourceReady, String> {
    let mut fetch = vec!["fetch", "--no-tags", "--quiet", spec.remote.as_str()];
    fetch.push(&spec.fetch_refspec);
    fetch.extend(spec.extra_fetch_refspecs.iter().map(String::as_str));
    git(&spec.repo_path, &fetch, FETCH_TIMEOUT).map_err(|err| format!("fetch: {err}"))?;
    // Work already started elsewhere (another tool, another Herdr) is continued, not
    // duplicated on a second branch.
    if let Some(key) = &spec.adopt_branch_for {
        match find_existing_work(
            &spec.repo_path,
            key,
            base_branch_name(&spec.base_ref),
            &spec.base_ref,
        )? {
            Some((branch, Some(path))) => {
                return Ok(SourceReady::ExistingWorktree { path, branch })
            }
            Some((branch, None)) => {
                return Ok(SourceReady::CreateWorktree {
                    branch,
                    base: spec.base_ref.clone(),
                })
            }
            None => {}
        }
    }
    if !branch_exists(&spec.repo_path, &spec.branch)? {
        return Ok(SourceReady::CreateWorktree {
            branch: spec.branch.clone(),
            base: spec.base_ref.clone(),
        });
    }
    if let Some(path) = worktree_for_branch(&spec.repo_path, &spec.branch)? {
        return Ok(SourceReady::ExistingWorktree {
            path,
            branch: spec.branch.clone(),
        });
    }
    if spec.reuse_branch {
        // Local commits on the branch stay; the fetched remote state is only the base
        // for a branch that did not exist.
        return Ok(SourceReady::CreateWorktree {
            branch: spec.branch.clone(),
            base: spec.base_ref.clone(),
        });
    }
    // A kept review branch without a worktree: continue from it on a fresh branch so
    // Herdr creates the worktree (and its hooks run) without discarding the kept work.
    for suffix in 2..=MAX_BRANCH_SUFFIX {
        let branch = format!("{}-{suffix}", spec.branch);
        if !branch_exists(&spec.repo_path, &branch)? {
            return Ok(SourceReady::CreateWorktree {
                branch,
                base: spec.branch.clone(),
            });
        }
    }
    Err(format!(
        "branches {0} to {0}-{MAX_BRANCH_SUFFIX} already exist",
        spec.branch
    ))
}

/// Prepares the workspace source: fetches for a worktree, or downloads the file.
pub(crate) fn prepare_source(source: &WorkspaceSource) -> Result<SourceReady, String> {
    match source {
        WorkspaceSource::Worktree(spec) => prepare_worktree(spec),
        WorkspaceSource::Download(spec) => download(spec).map(|()| SourceReady::Downloaded),
    }
}

/// Runs the download command and stores its output in the scratch directory.
fn download(spec: &DownloadSpec) -> Result<(), String> {
    std::fs::create_dir_all(&spec.directory)
        .map_err(|err| format!("{}: {err}", spec.directory.display()))?;
    let mut command = crate::noninteractive_process::command(&spec.program);
    command.args(&spec.args).current_dir(&spec.directory);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let output = match run_with_timeout(command, DOWNLOAD_TIMEOUT) {
        Ok(output) if output.status.success() => output,
        Ok(output) => return Err(format!("download: {}", failure_detail(&output))),
        Err(err) => return Err(format!("download: {err}")),
    };
    let path = spec.directory.join(&spec.file_name);
    std::fs::write(&path, &output.stdout).map_err(|err| format!("{}: {err}", path.display()))
}

/// Deletes a local review branch after its worktree was removed.
pub(crate) fn delete_branch(repo: &Path, branch: &str) -> Result<(), String> {
    git(repo, &["branch", "-D", "--quiet", branch], GIT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_items::source::WorkspaceLayout;

    fn plan(agent: &str) -> ProvisionPlan {
        ProvisionPlan {
            source: WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: PathBuf::from("/repo"),
                remote: "origin".into(),
                fetch_refspec: "+refs/pull/1/head:refs/herdr/pull/1".into(),
                base_ref: "refs/herdr/pull/1".into(),
                branch: "review/pr-1".into(),
                reuse_branch: false,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: None,
            }),
            workspace_label: "#1 Title".into(),
            agent_name_hint: "review-1".into(),
            brief: "brief".into(),
            layout: WorkspaceLayout {
                agent: agent.into(),
                agent_args: Vec::new(),
                editor_command: String::new(),
                lazygit_command: String::new(),
                diff_command: String::new(),
                review_command: String::new(),
            },
            delete_branch: true,
        }
    }

    fn statuses(progress: &WorkItemProvisioningInfo) -> Vec<WorkItemStepStatus> {
        progress.steps.iter().map(|info| info.status).collect()
    }

    #[test]
    fn initial_progress_skips_the_agent_without_one() {
        let progress = initial_progress(&plan(""));
        use WorkItemStepStatus::{Running, Skipped};
        assert_eq!(statuses(&progress), vec![Running, Skipped]);
        assert_eq!(progress.steps[0].label, "Worktree created");
        assert!(!progress.finished);
    }

    #[test]
    fn checkout_failure_skips_the_rest_and_finishes() {
        let mut progress = initial_progress(&plan("claude"));
        fail_checkout(&mut progress, "fetch: denied".into());
        use WorkItemStepStatus::{Failed, Skipped};
        assert_eq!(statuses(&progress), vec![Failed, Skipped]);
        assert!(progress.finished);
    }

    #[test]
    fn finished_only_once_every_step_is_terminal() {
        let mut progress = initial_progress(&plan("claude"));
        set_step(
            &mut progress,
            WorkItemStep::Checkout,
            WorkItemStepStatus::Done,
            None,
        );
        assert!(!progress.finished);
        set_step(
            &mut progress,
            WorkItemStep::AgentBrief,
            WorkItemStepStatus::Done,
            None,
        );
        assert!(progress.finished);
    }

    #[test]
    fn download_source_labels_the_first_step() {
        let mut download = plan("claude");
        download.source = WorkspaceSource::Download(DownloadSpec {
            directory: PathBuf::from("/worktrees/repo/pr-1-agent"),
            program: "gh".into(),
            args: Vec::new(),
            env: Vec::new(),
            file_name: "pr-1.diff".into(),
        });
        assert_eq!(
            initial_progress(&download).steps[0].label,
            "Diff downloaded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn download_writes_command_output_into_the_scratch_directory() {
        let directory =
            std::env::temp_dir().join(format!("herdr-work-items-download-{}", std::process::id()));
        let spec = DownloadSpec {
            directory: directory.join("pr-1-agent"),
            program: "printf".into(),
            args: vec!["diff --git a/x b/x\\n".into()],
            env: vec![("GH_PROMPT_DISABLED".into(), "1".into())],
            file_name: "pr-1.diff".into(),
        };
        let ready = prepare_source(&WorkspaceSource::Download(spec.clone())).expect("download");
        let written = std::fs::read_to_string(spec.directory.join("pr-1.diff")).expect("file");
        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(ready, SourceReady::Downloaded);
        assert_eq!(written, "diff --git a/x b/x\n");
    }

    #[cfg(unix)]
    fn test_repo(name: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!(
            "herdr-work-items-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec![
                "-c",
                "user.name=h",
                "-c",
                "user.email=h@example.test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "base",
            ],
            vec!["update-ref", "refs/pull/1/head", "HEAD"],
        ] {
            git(&repo, &args, GIT_TIMEOUT).expect("git setup");
        }
        repo
    }

    #[cfg(unix)]
    fn spec(repo: &Path) -> WorktreeSpec {
        WorktreeSpec {
            repo_path: repo.to_path_buf(),
            remote: repo.display().to_string(),
            fetch_refspec: "+refs/pull/1/head:refs/herdr/pull/1".into(),
            base_ref: "refs/herdr/pull/1".into(),
            branch: "review/pr-1".into(),
            reuse_branch: false,
            extra_fetch_refspecs: Vec::new(),
            adopt_branch_for: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn extra_refspecs_are_fetched_with_the_review_ref() {
        let repo = test_repo("extra");
        prepare_worktree(&WorktreeSpec {
            extra_fetch_refspecs: vec!["+HEAD:refs/herdr/base/main".into()],
            adopt_branch_for: None,
            ..spec(&repo)
        })
        .expect("prepared");
        let base = git_output(&repo, &["rev-parse", "refs/herdr/base/main"], GIT_TIMEOUT).unwrap();
        let head = git_output(&repo, &["rev-parse", "HEAD"], GIT_TIMEOUT).unwrap();
        let _ = std::fs::remove_dir_all(&repo);
        assert!(base.status.success());
        assert_eq!(base.stdout, head.stdout);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_review_creates_the_branch_from_the_fetched_ref() {
        let repo = test_repo("fresh");
        let ready = prepare_worktree(&spec(&repo));
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(
            ready,
            Ok(SourceReady::CreateWorktree {
                branch: "review/pr-1".into(),
                base: "refs/herdr/pull/1".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn kept_branch_without_worktree_continues_on_a_suffixed_branch() {
        let repo = test_repo("kept");
        git(&repo, &["branch", "review/pr-1", "HEAD"], GIT_TIMEOUT).unwrap();
        let ready = prepare_worktree(&spec(&repo));
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(
            ready,
            Ok(SourceReady::CreateWorktree {
                branch: "review/pr-1-2".into(),
                base: "review/pr-1".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_request_branch_is_reused_when_it_exists_locally() {
        let repo = test_repo("reuse");
        git(&repo, &["branch", "feature", "HEAD"], GIT_TIMEOUT).unwrap();
        let ready = prepare_worktree(&WorktreeSpec {
            branch: "feature".into(),
            reuse_branch: true,
            extra_fetch_refspecs: Vec::new(),
            adopt_branch_for: None,
            ..spec(&repo)
        });
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(
            ready,
            Ok(SourceReady::CreateWorktree {
                branch: "feature".into(),
                base: "refs/herdr/pull/1".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn checked_out_branch_reopens_its_worktree() {
        let repo = test_repo("reopen");
        let checkout = repo.with_extension("checkout");
        let checkout_arg = checkout.display().to_string();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "review/pr-1",
                &checkout_arg,
                "HEAD",
            ],
            GIT_TIMEOUT,
        )
        .unwrap();
        let ready = prepare_worktree(&spec(&repo));
        let expected = crate::worktree::canonical_or_original(&checkout);
        let found = match &ready {
            Ok(SourceReady::ExistingWorktree { path, .. }) => {
                Some(crate::worktree::canonical_or_original(path))
            }
            _ => None,
        };
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&checkout);
        assert_eq!(found, Some(expected), "{ready:?}");
    }

    #[test]
    fn issue_keys_match_whole_keys_only() {
        assert!(branch_matches_key(
            "ar/tech-2096-show-full-pricing",
            "TECH-2096"
        ));
        assert!(branch_matches_key("TECH-2096", "tech-2096"));
        assert!(!branch_matches_key("ar/tech-20960-other", "TECH-2096"));
        assert!(!branch_matches_key("ar/hightech-2096", "TECH-2096"));
        assert!(!branch_matches_key("main", ""));
    }

    #[cfg(unix)]
    #[test]
    fn existing_work_for_the_key_is_continued() {
        let repo = test_repo("adopt");
        let adopt = |repo: &Path| {
            prepare_worktree(&WorktreeSpec {
                branch: "tech-7-new".into(),
                adopt_branch_for: Some("TECH-7".into()),
                reuse_branch: true,
                ..spec(repo)
            })
        };
        git(&repo, &["branch", "ar/tech-7-started", "HEAD"], GIT_TIMEOUT).unwrap();
        let branch_only = adopt(&repo);
        let checkout = repo.with_extension("adopt-checkout");
        let checkout_arg = checkout.display().to_string();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                &checkout_arg,
                "ar/tech-7-started",
            ],
            GIT_TIMEOUT,
        )
        .unwrap();
        let checked_out = adopt(&repo);
        let expected_path = crate::worktree::canonical_or_original(&checkout);
        let found_path = match &checked_out {
            Ok(SourceReady::ExistingWorktree { path, .. }) => {
                Some(crate::worktree::canonical_or_original(path))
            }
            _ => None,
        };
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&checkout);
        assert_eq!(
            branch_only,
            Ok(SourceReady::CreateWorktree {
                branch: "ar/tech-7-started".into(),
                base: "refs/herdr/pull/1".into(),
            })
        );
        let Ok(SourceReady::ExistingWorktree { branch, .. }) = &checked_out else {
            panic!("{checked_out:?}");
        };
        assert_eq!(branch, "ar/tech-7-started");
        assert_eq!(found_path, Some(expected_path));
    }

    #[cfg(unix)]
    #[test]
    fn unmerged_commits_naming_the_key_find_its_branch() {
        let repo = test_repo("commits");
        let commit = |message: &str| {
            git(
                &repo,
                &[
                    "-c",
                    "user.name=h",
                    "-c",
                    "user.email=h@example.test",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    message,
                ],
                GIT_TIMEOUT,
            )
            .unwrap()
        };
        let base = String::from_utf8(
            git_output(&repo, &["rev-parse", "--abbrev-ref", "HEAD"], GIT_TIMEOUT)
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // TECH-8 already landed on the base; a later branch contains it but is not its work.
        commit("[TECH-8] merged earlier");
        git(&repo, &["branch", "ar/unrelated"], GIT_TIMEOUT).unwrap();
        git(
            &repo,
            &["checkout", "--quiet", "-b", "ar/full-pricing"],
            GIT_TIMEOUT,
        )
        .unwrap();
        commit("[TECH-9] Show full pricing");
        git(&repo, &["checkout", "--quiet", &base], GIT_TIMEOUT).unwrap();

        let found = find_existing_work(&repo, "tech-9", &base, &base);
        let merged = find_existing_work(&repo, "TECH-8", &base, &base);
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(found, Ok(Some(("ar/full-pricing".to_string(), None))));
        assert_eq!(merged, Ok(None));
    }
}
