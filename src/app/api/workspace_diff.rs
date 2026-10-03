use crate::api::schema::{Method, Request, ResponseResult};
use crate::app::App;
use crate::workspace_diff::{self, DiffRequest};

use super::responses::{encode_error, encode_success};

impl App {
    /// Handles `workspace.diff`. The app thread only finds the workspace's directory; Git
    /// runs on a thread of its own, which replies through `respond_to` when it is done, so
    /// a slow repository never stalls the app loop.
    pub(crate) fn handle_deferred_workspace_diff_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let Method::WorkspaceDiff(params) = request.method else {
            return false;
        };
        let id = request.id;
        let Some(ws_idx) = self
            .parse_workspace_id(&params.workspace_id)
            .filter(|&ws_idx| self.state.workspaces.get(ws_idx).is_some())
        else {
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                format!("workspace {} not found", params.workspace_id),
            ));
            return true;
        };
        let ws = &self.state.workspaces[ws_idx];
        let workspace_id = ws.id.clone();
        // A worktree workspace's checkout, else where its first pane is.
        let directory = ws
            .worktree_space()
            .map(|space| space.checkout_path.clone())
            .or_else(|| {
                ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            })
            .filter(|directory| !directory.as_os_str().is_empty());
        let Some(directory) = directory else {
            let _ = respond_to.send(encode_success(
                id,
                ResponseResult::WorkspaceDiff {
                    diff: workspace_diff::no_directory(&workspace_id),
                },
            ));
            return true;
        };
        let Ok(permit) = self.workspace_diff_slots.clone().try_acquire_owned() else {
            let _ = respond_to.send(encode_error(
                id,
                "worktree_busy",
                "too many workspace diffs are pending; retry shortly",
            ));
            return true;
        };
        let options = DiffRequest {
            path: params.path,
            summary_only: params.summary_only,
        };
        let spawn_error_response = respond_to.clone();
        let spawn_error_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("workspace-diff".into())
            .spawn(move || {
                // Held until the reply is sent, so the limit counts running diffs.
                let _permit = permit;
                let diff = workspace_diff::read_workspace_diff(&workspace_id, &directory, &options);
                let _ = respond_to.send(encode_success(id, ResponseResult::WorkspaceDiff { diff }));
            });
        if let Err(err) = spawned {
            let _ = spawn_error_response.send(encode_error(
                spawn_error_id,
                "workspace_diff_failed",
                format!("could not start the diff: {err}"),
            ));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::api::schema::{
        ErrorResponse, Method, Request, ResponseResult, SuccessResponse, WorkspaceDiffFileStatus,
        WorkspaceDiffParams, WorkspaceDiffStatus,
    };
    use crate::app::App;
    use crate::{config::Config, workspace::Workspace};

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {}", args.join(" "));
    }

    fn committed_repo(name: &str) -> PathBuf {
        let repo = unique_temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "herdr@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Herdr Test"]);
        std::fs::write(repo.join("README.md"), "one\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    fn test_app(directory: &Path) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("main");
        workspace.identity_cwd = directory.to_path_buf();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app
    }

    fn diff_request(workspace_id: &str, path: Option<&str>, summary_only: bool) -> Request {
        Request {
            id: "req".into(),
            method: Method::WorkspaceDiff(WorkspaceDiffParams {
                workspace_id: workspace_id.into(),
                path: path.map(Into::into),
                summary_only,
            }),
        }
    }

    fn ask(app: &mut App, request: Request) -> String {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_workspace_diff_api_request(request, respond_to));
        response_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("workspace.diff answers")
    }

    fn diff_of(response: &str) -> crate::api::schema::WorkspaceDiffInfo {
        let success: SuccessResponse = serde_json::from_str(response).unwrap();
        let ResponseResult::WorkspaceDiff { diff } = success.result else {
            panic!("not a workspace_diff result: {response}");
        };
        diff
    }

    #[tokio::test]
    async fn answers_with_the_changes_of_the_workspace_directory() {
        let repo = committed_repo("workspace-diff-app");
        std::fs::write(repo.join("README.md"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        let mut app = test_app(&repo);
        let workspace_id = app.state.workspaces[0].id.clone();

        let diff = diff_of(&ask(&mut app, diff_request(&workspace_id, None, false)));

        assert_eq!(diff.status, WorkspaceDiffStatus::Available);
        assert_eq!(diff.workspace_id, workspace_id);
        assert_eq!(diff.branch.as_deref(), Some("main"));
        let by_path = |path: &str| diff.files.iter().find(|file| file.path == path).unwrap();
        assert_eq!(
            by_path("README.md").status,
            WorkspaceDiffFileStatus::Modified
        );
        assert_eq!(by_path("README.md").additions, Some(1));
        assert_eq!(
            by_path("new.txt").status,
            WorkspaceDiffFileStatus::Untracked
        );
        assert_eq!(
            by_path("new.txt").patch.as_deref(),
            Some("@@ -0,0 +1,1 @@\n+fresh\n")
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn passes_path_and_summary_only_to_the_diff() {
        let repo = committed_repo("workspace-diff-app-options");
        std::fs::write(repo.join("README.md"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        let mut app = test_app(&repo);
        let workspace_id = app.state.workspaces[0].id.clone();

        let one = diff_of(&ask(
            &mut app,
            diff_request(&workspace_id, Some("new.txt"), false),
        ));
        assert_eq!(one.files.len(), 1);
        assert_eq!(one.files[0].path, "new.txt");

        let summary = diff_of(&ask(&mut app, diff_request(&workspace_id, None, true)));
        assert_eq!(summary.files.len(), 2);
        assert!(summary.files.iter().all(|file| file.patch.is_none()));
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn an_unknown_workspace_is_the_error_workspace_get_gives() {
        let repo = committed_repo("workspace-diff-app-unknown");
        let mut app = test_app(&repo);

        let response = ask(&mut app, diff_request("w-nope", None, false));

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "workspace_not_found");
        assert_eq!(error.error.message, "workspace w-nope not found");
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn a_workspace_without_a_directory_answers_no_directory() {
        let repo = committed_repo("workspace-diff-app-nodir");
        let mut app = test_app(&repo);
        app.state.workspaces[0].tabs.clear();
        app.state.workspaces[0].identity_cwd = PathBuf::new();
        let workspace_id = app.state.workspaces[0].id.clone();

        let diff = diff_of(&ask(&mut app, diff_request(&workspace_id, None, false)));

        assert_eq!(diff.status, WorkspaceDiffStatus::NoDirectory);
        assert_eq!(diff.directory, None);
        assert!(diff.files.is_empty());
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn a_worktree_workspace_is_diffed_in_its_checkout() {
        let repo = committed_repo("workspace-diff-app-worktree");
        let checkout = unique_temp_path("workspace-diff-app-worktree-checkout");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::write(checkout.join("only-here.txt"), "x\n").unwrap();
        let mut app = test_app(&repo);
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: repo.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        });
        let workspace_id = app.state.workspaces[0].id.clone();

        let diff = diff_of(&ask(&mut app, diff_request(&workspace_id, None, false)));

        assert_eq!(diff.branch.as_deref(), Some("feature"));
        let paths: Vec<_> = diff.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["only-here.txt"]);
        run_git(
            &repo,
            &["worktree", "remove", "--force", checkout.to_str().unwrap()],
        );
        let _ = std::fs::remove_dir_all(repo);
    }

    #[tokio::test]
    async fn too_many_diffs_in_flight_answer_busy_and_release_their_slot() {
        let repo = committed_repo("workspace-diff-app-busy");
        let mut app = test_app(&repo);
        app.workspace_diff_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let workspace_id = app.state.workspaces[0].id.clone();
        let held = app
            .workspace_diff_slots
            .clone()
            .try_acquire_owned()
            .unwrap();

        let busy = ask(&mut app, diff_request(&workspace_id, None, false));
        let error: ErrorResponse = serde_json::from_str(&busy).unwrap();
        assert_eq!(error.error.code, "worktree_busy");

        drop(held);
        let diff = diff_of(&ask(&mut app, diff_request(&workspace_id, None, false)));
        assert_eq!(diff.status, WorkspaceDiffStatus::Available);
        // The slot comes back once the reply is out.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.workspace_diff_slots.available_permits() != 1 {
            assert!(std::time::Instant::now() < deadline, "slot not released");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(repo);
    }
}
