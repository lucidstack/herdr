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
    /// Create a worktree on a new `branch` starting at `base`.
    NewBranch { branch: String, base: String },
    /// The review branch is already checked out here; reopen it.
    ExistingWorktree(PathBuf),
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

/// Path of the worktree that has `branch` checked out, if any.
fn worktree_for_branch(repo: &Path, branch: &str) -> Result<Option<PathBuf>, String> {
    let output = git_output(repo, &["worktree", "list", "--porcelain"], GIT_TIMEOUT)?;
    if !output.status.success() {
        return Err(failure_detail(&output));
    }
    let wanted = format!("branch refs/heads/{branch}");
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut path = None;
    for line in listing.lines() {
        if let Some(worktree) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(worktree));
        } else if line == wanted {
            return Ok(path);
        }
    }
    Ok(None)
}

/// Fetches the change and decides how Herdr should create or reopen its worktree.
fn prepare_worktree(spec: &WorktreeSpec) -> Result<SourceReady, String> {
    git(
        &spec.repo_path,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            &spec.remote,
            &spec.fetch_refspec,
        ],
        FETCH_TIMEOUT,
    )
    .map_err(|err| format!("fetch: {err}"))?;
    if !branch_exists(&spec.repo_path, &spec.branch)? {
        return Ok(SourceReady::NewBranch {
            branch: spec.branch.clone(),
            base: spec.base_ref.clone(),
        });
    }
    if let Some(path) = worktree_for_branch(&spec.repo_path, &spec.branch)? {
        return Ok(SourceReady::ExistingWorktree(path));
    }
    // A kept review branch without a worktree: continue from it on a fresh branch so
    // Herdr creates the worktree (and its hooks run) without discarding the kept work.
    for suffix in 2..=MAX_BRANCH_SUFFIX {
        let branch = format!("{}-{suffix}", spec.branch);
        if !branch_exists(&spec.repo_path, &branch)? {
            return Ok(SourceReady::NewBranch {
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
            }),
            workspace_label: "#1 Title".into(),
            agent_name_hint: "review-1".into(),
            brief: "brief".into(),
            layout: WorkspaceLayout {
                agent: agent.into(),
                editor_command: String::new(),
                lazygit_command: String::new(),
                diff_command: String::new(),
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
        }
    }

    #[cfg(unix)]
    #[test]
    fn fresh_review_creates_the_branch_from_the_fetched_ref() {
        let repo = test_repo("fresh");
        let ready = prepare_worktree(&spec(&repo));
        let _ = std::fs::remove_dir_all(&repo);
        assert_eq!(
            ready,
            Ok(SourceReady::NewBranch {
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
            Ok(SourceReady::NewBranch {
                branch: "review/pr-1-2".into(),
                base: "review/pr-1".into(),
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
            Ok(SourceReady::ExistingWorktree(path)) => {
                Some(crate::worktree::canonical_or_original(path))
            }
            _ => None,
        };
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&checkout);
        assert_eq!(found, Some(expected), "{ready:?}");
    }
}
