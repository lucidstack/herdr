//! Local provisioning of a work item: progress model and background workers.
//!
//! Progress transitions are pure; the workers block and must only run on
//! background threads.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::api::schema::{
    WorkItemProvisioningInfo, WorkItemStep, WorkItemStepInfo, WorkItemStepStatus,
};

use super::process::{failure_detail, run_with_timeout};
use super::source::{CheckoutSpec, DownloadSpec, ProvisionPlan, WorkspaceSource};

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Retry state for starting the agent or delivering its brief.
#[derive(Debug)]
pub(crate) struct AgentAttempt {
    pub started: Instant,
    pub next_attempt: Instant,
    /// Response channel of an in-flight `agent.prompt`.
    pub pending: Option<std::sync::mpsc::Receiver<String>>,
}

#[derive(Debug)]
pub(crate) struct ProvisionJob {
    pub job_id: u64,
    pub key: String,
    pub plan: ProvisionPlan,
    pub workspace_id: Option<String>,
    pub agent_pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub server_pane_id: Option<String>,
    /// Waiting for the shell in the agent pane to accept `agent.start`.
    pub agent_start: Option<AgentAttempt>,
    /// Waiting for the agent to accept its brief.
    pub brief: Option<AgentAttempt>,
}

impl ProvisionJob {
    pub(crate) fn next_attempt(&self) -> Option<Instant> {
        [self.agent_start.as_ref(), self.brief.as_ref()]
            .into_iter()
            .flatten()
            .map(|attempt| attempt.next_attempt)
            .min()
    }
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

/// Progress right after the user chose to review locally.
pub(crate) fn initial_progress(plan: &ProvisionPlan, agent: &str) -> WorkItemProvisioningInfo {
    use WorkItemStepStatus::{Pending, Running, Skipped};
    let (first_label, no_install) = match &plan.source {
        WorkspaceSource::Worktree(_) => ("Branch checked out", "no install_command configured"),
        WorkspaceSource::Download(_) => ("Diff downloaded", "no checkout"),
    };
    let dependencies = if plan.install_command.is_some() {
        step(
            WorkItemStep::Dependencies,
            "Dependencies installed",
            Pending,
            None,
        )
    } else {
        step(
            WorkItemStep::Dependencies,
            "Dependencies installed",
            Skipped,
            Some(no_install),
        )
    };
    let server = if plan.server.is_some() {
        step(WorkItemStep::Server, "Server running", Pending, None)
    } else {
        step(
            WorkItemStep::Server,
            "Server running",
            Skipped,
            Some(&plan.server_skip_reason),
        )
    };
    let agent_brief = if agent.is_empty() {
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
            dependencies,
            server,
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

/// Checkout failed: record it and skip everything that depends on it.
pub(crate) fn fail_checkout(progress: &mut WorkItemProvisioningInfo, detail: String) {
    set_step(
        progress,
        WorkItemStep::Checkout,
        WorkItemStepStatus::Failed,
        Some(detail),
    );
    end_unfinished(progress, WorkItemStepStatus::Skipped, "checkout failed");
}

/// Dependencies failed: record it and skip a server that would need them.
pub(crate) fn fail_dependencies(progress: &mut WorkItemProvisioningInfo, detail: String) {
    set_step(
        progress,
        WorkItemStep::Dependencies,
        WorkItemStepStatus::Failed,
        Some(detail),
    );
    if status(progress, WorkItemStep::Server) == Some(WorkItemStepStatus::Pending) {
        set_step(
            progress,
            WorkItemStep::Server,
            WorkItemStepStatus::Skipped,
            Some("dependencies failed".into()),
        );
    }
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

fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<(), String> {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    match run_with_timeout(command, timeout) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(failure_detail(&output)),
        Err(err) => Err(err.to_string()),
    }
}

/// Fetches the change and checks it out detached in its own worktree.
fn checkout(spec: &CheckoutSpec) -> Result<(), String> {
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
    if spec.checkout_path.join(".git").exists() {
        return git(
            &spec.checkout_path,
            &["checkout", "--detach", "--quiet", &spec.checkout_ref],
            WORKTREE_TIMEOUT,
        )
        .map_err(|err| format!("checkout: {err}"));
    }
    if let Some(parent) = spec.checkout_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("worktree add: {}: {err}", parent.display()))?;
    }
    let checkout_path = spec.checkout_path.to_string_lossy();
    git(
        &spec.repo_path,
        &[
            "worktree",
            "add",
            "--detach",
            &checkout_path,
            &spec.checkout_ref,
        ],
        WORKTREE_TIMEOUT,
    )
    .map_err(|err| format!("worktree add: {err}"))
}

/// Prepares the workspace directory: a worktree checkout or a downloaded file.
pub(crate) fn prepare_source(source: &WorkspaceSource) -> Result<(), String> {
    match source {
        WorkspaceSource::Worktree(spec) => checkout(spec),
        WorkspaceSource::Download(spec) => download(spec),
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

/// Runs the dependency install command in `cwd` through the user's shell.
pub(crate) fn install(command: &str, cwd: &Path) -> Result<(), String> {
    let mut process = crate::platform::detached_custom_command_process(command);
    process.current_dir(cwd);
    match run_with_timeout(process, INSTALL_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(match output.status.code() {
            Some(code) => format!("exit {code}: {}", failure_detail(&output)),
            None => failure_detail(&output),
        }),
        Err(err) => Err(err.to_string()),
    }
}

/// Waits until something accepts connections on the local `port`.
pub(crate) fn probe(port: u16) -> Result<(), String> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let started = Instant::now();
    loop {
        if TcpStream::connect_timeout(&address, PROBE_CONNECT_TIMEOUT).is_ok() {
            return Ok(());
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            return Err(format!(
                "nothing listening on port {port} after {} s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_items::source::ServerPlan;
    use std::path::PathBuf;

    fn plan(install: bool, server: bool) -> ProvisionPlan {
        ProvisionPlan {
            source: WorkspaceSource::Worktree(CheckoutSpec {
                repo_path: PathBuf::from("/repo"),
                remote: "origin".into(),
                fetch_refspec: "+refs/pull/1/head:refs/herdr/pull/1".into(),
                checkout_ref: "refs/herdr/pull/1".into(),
                checkout_path: PathBuf::from("/worktrees/repo/pr-1"),
            }),
            workspace_label: "#1 Title".into(),
            agent_name_hint: "review-1".into(),
            brief: "brief".into(),
            install_command: install.then(|| "npm ci".into()),
            server: server.then(|| ServerPlan {
                command: "npm run dev".into(),
                port: Some(3000),
            }),
            server_skip_reason: "no front-end changes".into(),
        }
    }

    fn statuses(progress: &WorkItemProvisioningInfo) -> Vec<WorkItemStepStatus> {
        progress.steps.iter().map(|info| info.status).collect()
    }

    #[test]
    fn initial_progress_skips_unconfigured_steps_with_reasons() {
        let progress = initial_progress(&plan(false, false), "");
        use WorkItemStepStatus::{Running, Skipped};
        assert_eq!(
            statuses(&progress),
            vec![Running, Skipped, Skipped, Skipped]
        );
        assert_eq!(
            progress.steps[2].detail.as_deref(),
            Some("no front-end changes")
        );
        assert!(!progress.finished);
    }

    #[test]
    fn checkout_failure_skips_the_rest_and_finishes() {
        let mut progress = initial_progress(&plan(true, true), "claude");
        fail_checkout(&mut progress, "fetch: denied".into());
        use WorkItemStepStatus::{Failed, Skipped};
        assert_eq!(statuses(&progress), vec![Failed, Skipped, Skipped, Skipped]);
        assert_eq!(progress.steps[3].detail.as_deref(), Some("checkout failed"));
        assert!(progress.finished);
    }

    #[test]
    fn dependency_failure_skips_a_pending_server() {
        let mut progress = initial_progress(&plan(true, true), "claude");
        fail_dependencies(&mut progress, "exit 1".into());
        assert_eq!(
            status(&progress, WorkItemStep::Server),
            Some(WorkItemStepStatus::Skipped)
        );
        assert_eq!(
            progress.steps[2].detail.as_deref(),
            Some("dependencies failed")
        );
    }

    #[test]
    fn finished_only_once_every_step_is_terminal() {
        let mut progress = initial_progress(&plan(false, false), "claude");
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
    fn download_source_labels_the_first_step_and_skips_install_without_checkout() {
        let mut download = plan(false, false);
        download.source = WorkspaceSource::Download(DownloadSpec {
            directory: PathBuf::from("/worktrees/repo/pr-1-agent"),
            program: "gh".into(),
            args: Vec::new(),
            env: Vec::new(),
            file_name: "pr-1.diff".into(),
        });
        let progress = initial_progress(&download, "claude");
        assert_eq!(progress.steps[0].label, "Diff downloaded");
        assert_eq!(progress.steps[1].detail.as_deref(), Some("no checkout"));
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
        prepare_source(&WorkspaceSource::Download(spec.clone())).expect("download");
        let written = std::fs::read_to_string(spec.directory.join("pr-1.diff")).expect("file");
        let _ = std::fs::remove_dir_all(&directory);
        assert_eq!(written, "diff --git a/x b/x\n");
    }
}
