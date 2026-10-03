//! `workspace.diff`: what changed in a workspace's checkout.
//!
//! Git runs on the caller's thread, one bounded process after another, so the caller is a
//! background thread and never the app loop. The working tree, with what is committed on
//! the branch, staged, unstaged and untracked, is compared with the merge-base of `HEAD`
//! and the branch the work started from.

mod parse;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use crate::api::schema::{
    WorkspaceDiffBase, WorkspaceDiffFile, WorkspaceDiffFileStatus, WorkspaceDiffInfo,
    WorkspaceDiffStatus,
};
use crate::work_items::process::{
    failure_detail, first_line, run_with_timeout, run_with_timeout_capped,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most files a reply lists.
const MAX_FILES: usize = 2000;
/// The most patch a file, and a whole reply, carries.
const FILE_PATCH_LIMIT: usize = 128 * 1024;
const REPLY_PATCH_LIMIT: usize = 512 * 1024;
/// The same, when the request names one file.
const SINGLE_FILE_PATCH_LIMIT: usize = 2 * 1024 * 1024;
/// How much of an untracked file is read, to count its lines and write its patch.
const UNTRACKED_READ_LIMIT: usize = 256 * 1024;
/// The longest message of Git's that is passed on.
const MAX_ERROR_CHARS: usize = 200;
/// How much of a command's output is kept. Past it the command is stopped, and what is
/// missing counts as cut off.
const LIST_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const PATCH_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

/// What a request asks for.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiffRequest {
    /// Only this file, as its `path` or its `old_path` names it.
    pub path: Option<String>,
    /// No patches, for the counts.
    pub summary_only: bool,
}

/// The reply for a workspace without a directory.
pub(crate) fn no_directory(workspace_id: &str) -> WorkspaceDiffInfo {
    empty(workspace_id, None, WorkspaceDiffStatus::NoDirectory)
}

/// What changed in the checkout `directory` belongs to. Blocking: runs Git.
pub(crate) fn read_workspace_diff(
    workspace_id: &str,
    directory: &Path,
    request: &DiffRequest,
) -> WorkspaceDiffInfo {
    let given = Some(directory.display().to_string());
    if !directory.is_dir() {
        return empty(workspace_id, given, WorkspaceDiffStatus::NoDirectory);
    }
    match read(workspace_id, directory, request) {
        Ok(info) => info,
        Err(Failure::NotARepository) => {
            empty(workspace_id, given, WorkspaceDiffStatus::NotARepository)
        }
        Err(Failure::Unreadable(message)) => WorkspaceDiffInfo {
            error: Some(message),
            ..empty(workspace_id, given, WorkspaceDiffStatus::Unreadable)
        },
    }
}

fn empty(
    workspace_id: &str,
    directory: Option<String>,
    status: WorkspaceDiffStatus,
) -> WorkspaceDiffInfo {
    WorkspaceDiffInfo {
        workspace_id: workspace_id.to_string(),
        status,
        directory,
        branch: None,
        base: None,
        head: None,
        additions: 0,
        deletions: 0,
        truncated: false,
        files: Vec::new(),
        error: None,
    }
}

#[derive(Debug)]
enum Failure {
    NotARepository,
    /// Git failed or timed out; Git's message.
    Unreadable(String),
}

/// A file about to be listed.
struct Entry {
    path: String,
    old_path: Option<String>,
    status: WorkspaceDiffFileStatus,
    additions: Option<u64>,
    deletions: Option<u64>,
    binary: bool,
    /// Git knows the file; its patch comes from `git diff`.
    tracked: bool,
}

impl Entry {
    /// The line Git starts this file's patch with.
    fn header(&self) -> String {
        parse::diff_header(self.old_path.as_deref().unwrap_or(&self.path), &self.path)
    }
}

struct Listing {
    entries: Vec<Entry>,
    /// Git printed more than was kept.
    cut: bool,
}

fn read(
    workspace_id: &str,
    directory: &Path,
    request: &DiffRequest,
) -> Result<WorkspaceDiffInfo, Failure> {
    let root = repository_root(directory)?;
    let head = head_commit(&root)?;
    let branch = checked_out_branch(&root)?;
    let base = match &head {
        Some(head) => find_base(&root, branch.as_deref(), head)?,
        None => None,
    };
    // What the working tree is compared with: the base, else what is committed, else, in a
    // repository without a commit, nothing at all.
    let target = match (&base, &head) {
        (Some(base), _) => base.commit.clone(),
        (None, Some(head)) => head.clone(),
        (None, None) => empty_tree(&root)?,
    };

    let tracked = tracked_entries(&root, &target)?;
    let untracked = untracked_entries(&root)?;
    let mut truncated = tracked.cut || untracked.cut;
    let mut entries = tracked.entries;
    entries.extend(untracked.entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(wanted) = request.path.as_deref() {
        entries.retain(|entry| entry.path == wanted || entry.old_path.as_deref() == Some(wanted));
    }
    if entries.len() > MAX_FILES {
        entries.truncate(MAX_FILES);
        truncated = true;
    }

    let single_file = request.path.is_some();
    let patches = if request.summary_only {
        None
    } else {
        tracked_patches(&root, &target, &entries, single_file)?
    };
    let chunks = patches
        .as_ref()
        .map(|(text, cut)| parse::split_patches(text, *cut));

    let mut budget = PatchBudget::new(single_file);
    let read_limit = if single_file {
        SINGLE_FILE_PATCH_LIMIT
    } else {
        UNTRACKED_READ_LIMIT
    };
    let mut files = Vec::with_capacity(entries.len());
    let (mut additions, mut deletions) = (0u64, 0u64);
    for entry in entries {
        let mut file = WorkspaceDiffFile {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            status: entry.status,
            additions: entry.additions,
            deletions: entry.deletions,
            binary: entry.binary,
            patch: None,
            patch_truncated: false,
        };
        if entry.tracked {
            if !entry.binary && !request.summary_only {
                match chunks
                    .as_ref()
                    .and_then(|chunks| chunks.patch(&entry.header()))
                {
                    Some(patch) => place(&mut file, patch, &mut budget),
                    None => file.patch_truncated = true,
                }
            }
        } else {
            match read_untracked(&root, &entry.path, read_limit) {
                Untracked::Gone => continue,
                Untracked::Binary => file.binary = true,
                Untracked::Unreadable | Untracked::TooLarge => {
                    file.patch_truncated = !request.summary_only;
                }
                Untracked::Text(content) => {
                    file.additions = Some(parse::count_added_lines(&content));
                    file.deletions = Some(0);
                    if !request.summary_only {
                        place(&mut file, parse::added_file_patch(&content), &mut budget);
                    }
                }
            }
        }
        additions += file.additions.unwrap_or(0);
        deletions += file.deletions.unwrap_or(0);
        files.push(file);
    }

    Ok(WorkspaceDiffInfo {
        workspace_id: workspace_id.to_string(),
        status: WorkspaceDiffStatus::Available,
        directory: Some(root.display().to_string()),
        branch,
        base: base.map(|base| WorkspaceDiffBase {
            ref_name: Some(base.ref_name),
            commit: base.commit,
        }),
        head,
        additions,
        deletions,
        truncated,
        files,
        error: None,
    })
}

/// What a reply may still carry of patches.
struct PatchBudget {
    file: usize,
    left: usize,
    /// A patch did not fit; the rest of the files come without one.
    exhausted: bool,
}

impl PatchBudget {
    fn new(single_file: bool) -> Self {
        if single_file {
            Self {
                file: SINGLE_FILE_PATCH_LIMIT,
                left: SINGLE_FILE_PATCH_LIMIT,
                exhausted: false,
            }
        } else {
            Self {
                file: FILE_PATCH_LIMIT,
                left: REPLY_PATCH_LIMIT,
                exhausted: false,
            }
        }
    }

    /// Whether a patch of `len` bytes goes into the reply, counting it against what is left.
    /// A patch that alone is too large is left out and costs nothing. One that no longer
    /// fits the reply ends the patches: everything after it comes without.
    fn admit(&mut self, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        if self.exhausted || len > self.file {
            return false;
        }
        if len > self.left {
            self.exhausted = true;
            return false;
        }
        self.left -= len;
        true
    }
}

fn place(file: &mut WorkspaceDiffFile, patch: String, budget: &mut PatchBudget) {
    if budget.admit(patch.len()) {
        file.patch = Some(patch);
    } else {
        file.patch_truncated = true;
    }
}

// ---- Running Git -------------------------------------------------------------------------

/// `git` for `root`: English messages, no prompts, and paths taken literally.
fn git_command(root: &Path) -> Command {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotepath=off", "--literal-pathspecs"])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

struct Run {
    output: Output,
    /// More output was printed than kept, and Git was stopped.
    cut: bool,
}

fn execute(command: Command, what: &str, cap: Option<usize>) -> Result<Run, Failure> {
    let ran = match cap {
        Some(cap) => run_with_timeout_capped(command, GIT_TIMEOUT, cap),
        None => run_with_timeout(command, GIT_TIMEOUT).map(|output| (output, false)),
    };
    match ran {
        Ok((output, cut)) => Ok(Run { output, cut }),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            Err(Failure::Unreadable(format!("git {what} {error}")))
        }
        Err(error) => Err(Failure::Unreadable(format!("could not run git: {error}"))),
    }
}

/// Git's complaint: its `fatal:` line, which names the cause, else the first line it printed.
fn detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("fatal:"))
        .map(|line| line.chars().take(MAX_ERROR_CHARS).collect())
        .or_else(|| first_line(&output.stderr))
        .unwrap_or_else(|| failure_detail(output))
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Runs a command whose failure is a failure of the diff, and keeps what it printed.
fn listing_output(command: Command, what: &str) -> Result<(Vec<u8>, bool), Failure> {
    let run = execute(command, what, Some(LIST_OUTPUT_LIMIT))?;
    if run.cut || run.output.status.success() {
        Ok((run.output.stdout, run.cut))
    } else {
        Err(Failure::Unreadable(detail(&run.output)))
    }
}

/// What a command that may find nothing printed: `None` when it exits with 1, as
/// `config --get`, `symbolic-ref -q` and `rev-parse -q --verify` do for what is not there.
fn optional_output(root: &Path, args: &[&str], what: &str) -> Result<Option<String>, Failure> {
    let mut command = git_command(root);
    command.args(args);
    let output = execute(command, what, None)?.output;
    if output.status.success() {
        let printed = text(&output);
        return Ok((!printed.is_empty()).then_some(printed));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(Failure::Unreadable(detail(&output)))
}

fn repository_root(directory: &Path) -> Result<PathBuf, Failure> {
    let mut command = git_command(directory);
    command.args(["rev-parse", "--show-toplevel"]);
    let output = execute(command, "rev-parse", None)?.output;
    if output.status.success() {
        let root = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        return if root.is_empty() {
            Err(Failure::NotARepository)
        } else {
            Ok(PathBuf::from(root))
        };
    }
    let message = detail(&output);
    // Git's two ways of saying the directory is not in a work tree: outside any
    // repository, and inside a `.git` directory or a bare repository.
    if message.contains("not a git repository") || message.contains("must be run in a work tree") {
        Err(Failure::NotARepository)
    } else {
        Err(Failure::Unreadable(message))
    }
}

/// The commit `HEAD` points at; `None` on a branch without a commit.
fn head_commit(root: &Path) -> Result<Option<String>, Failure> {
    optional_output(
        root,
        &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
        "rev-parse",
    )
}

/// The checked-out branch; `None` when `HEAD` is detached.
fn checked_out_branch(root: &Path) -> Result<Option<String>, Failure> {
    optional_output(
        root,
        &["symbolic-ref", "--short", "-q", "HEAD"],
        "symbolic-ref",
    )
}

fn empty_tree(root: &Path) -> Result<String, Failure> {
    let mut command = git_command(root);
    command.args(["hash-object", "-t", "tree", "--stdin"]);
    let output = execute(command, "hash-object", None)?.output;
    if output.status.success() {
        Ok(text(&output))
    } else {
        Err(Failure::Unreadable(detail(&output)))
    }
}

// ---- The base ----------------------------------------------------------------------------

struct Base {
    ref_name: String,
    commit: String,
}

/// The branch the work started from, and where `HEAD` left it: the base recorded for the
/// branch (`branch.<name>.herdrBase`, which `worktree.create` writes), else the default
/// branch of the branch's remote, else a local `main`, else a local `master`. A candidate
/// that is gone, or shares no history with `HEAD`, gives way to the next one.
fn find_base(root: &Path, branch: Option<&str>, head: &str) -> Result<Option<Base>, Failure> {
    if let Some(branch) = branch {
        let key = format!("branch.{branch}.herdrBase");
        if let Some(recorded) = optional_output(root, &["config", "--get", &key], "config")? {
            if let Some(base) = merge_base(root, head, &recorded)? {
                return Ok(Some(base));
            }
        }
    }

    let configured_remote = match branch {
        Some(branch) => {
            let key = format!("branch.{branch}.remote");
            optional_output(root, &["config", "--get", &key], "config")?
        }
        None => None,
    };
    // `.` names the repository itself, which has no `refs/remotes` of its own.
    let remote = configured_remote
        .filter(|remote| remote != ".")
        .unwrap_or_else(|| "origin".to_string());
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    if let Some(default_branch) =
        optional_output(root, &["symbolic-ref", "-q", &remote_head], "symbolic-ref")?
    {
        if let Some(base) = merge_base(root, head, &default_branch)? {
            return Ok(Some(base));
        }
    }

    for local in ["refs/heads/main", "refs/heads/master"] {
        if let Some(base) = merge_base(root, head, local)? {
            return Ok(Some(base));
        }
    }
    Ok(None)
}

fn merge_base(root: &Path, head: &str, reference: &str) -> Result<Option<Base>, Failure> {
    // A reference that looks like an option is not one this could have been given.
    if reference.starts_with('-') {
        return Ok(None);
    }
    let mut command = git_command(root);
    command.args(["merge-base", head, reference]);
    let output = execute(command, "merge-base", None)?.output;
    // Exit 1: no common ancestor. Exit 128: the reference is gone. Either way, not a base.
    if !output.status.success() {
        return Ok(None);
    }
    let commit = text(&output);
    if commit.is_empty() {
        return Ok(None);
    }
    Ok(Some(Base {
        ref_name: parse::short_ref_name(reference).to_string(),
        commit,
    }))
}

// ---- The files ---------------------------------------------------------------------------

/// `git diff` against `target`: renames detected, the same flags for every call so that
/// they all see the same files.
fn diff_command(root: &Path, target: &str, format: &[&str], pathspecs: &[&str]) -> Command {
    let mut command = git_command(root);
    command
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "-U3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--submodule=short",
        ])
        .args(format)
        .arg(target)
        .arg("--")
        .args(pathspecs);
    command
}

/// The files Git tracks that differ from `target` in the working tree: committed on the
/// branch, staged and unstaged.
fn tracked_entries(root: &Path, target: &str) -> Result<Listing, Failure> {
    let (names, names_cut) = listing_output(
        diff_command(root, target, &["-z", "--name-status"], &[]),
        "diff",
    )?;
    let (counts, counts_cut) = listing_output(
        diff_command(root, target, &["-z", "--numstat"], &[]),
        "diff",
    )?;
    let counts = parse::parse_numstat(&counts);
    let entries = parse::parse_name_status(&names)
        .into_iter()
        .map(|listed| {
            let counted = counts.get(&listed.path).copied();
            Entry {
                path: listed.path,
                old_path: listed.old_path,
                status: listed.status,
                additions: counted.and_then(|counted| counted.additions),
                deletions: counted.and_then(|counted| counted.deletions),
                binary: counted.is_some_and(|counted| counted.binary),
                tracked: true,
            }
        })
        .collect();
    Ok(Listing {
        entries,
        cut: names_cut || counts_cut,
    })
}

fn untracked_entries(root: &Path) -> Result<Listing, Failure> {
    let mut command = git_command(root);
    command.args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let (listed, cut) = listing_output(command, "ls-files")?;
    let entries = parse::parse_file_list(&listed)
        .into_iter()
        .map(|path| Entry {
            path,
            old_path: None,
            status: WorkspaceDiffFileStatus::Untracked,
            additions: None,
            deletions: None,
            binary: false,
            tracked: false,
        })
        .collect();
    Ok(Listing { entries, cut })
}

/// The output of one `git diff` for the tracked text files among `entries`, and whether it
/// was cut off. `None` when there are none. With `single_file`, Git is limited to the
/// file's paths, so that nothing else in the checkout takes up the output.
fn tracked_patches(
    root: &Path,
    target: &str,
    entries: &[Entry],
    single_file: bool,
) -> Result<Option<(String, bool)>, Failure> {
    let wanted: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.tracked && !entry.binary)
        .collect();
    if wanted.is_empty() {
        return Ok(None);
    }
    let mut pathspecs: Vec<&str> = Vec::new();
    if single_file {
        for entry in &wanted {
            pathspecs.extend(entry.old_path.as_deref());
            pathspecs.push(&entry.path);
        }
    }
    let run = execute(
        diff_command(root, target, &[], &pathspecs),
        "diff",
        Some(PATCH_OUTPUT_LIMIT),
    )?;
    if !run.cut && !run.output.status.success() {
        return Err(Failure::Unreadable(detail(&run.output)));
    }
    Ok(Some((
        String::from_utf8_lossy(&run.output.stdout).into_owned(),
        run.cut,
    )))
}

enum Untracked {
    /// Not there any more, or neither a regular file nor a symbolic link.
    Gone,
    Binary,
    Text(Vec<u8>),
    TooLarge,
    Unreadable,
}

/// Reads an untracked file the way Git would show it as new: a symbolic link as the path it
/// points to, never followed.
fn read_untracked(root: &Path, path: &str, read_limit: usize) -> Untracked {
    let full = root.join(path);
    let Ok(metadata) = std::fs::symlink_metadata(&full) else {
        return Untracked::Gone;
    };
    if metadata.file_type().is_symlink() {
        return match std::fs::read_link(&full) {
            Ok(target) => Untracked::Text(target.to_string_lossy().into_owned().into_bytes()),
            Err(_) => Untracked::Unreadable,
        };
    }
    if !metadata.is_file() {
        return Untracked::Gone;
    }
    let Ok(mut file) = File::open(&full) else {
        return Untracked::Unreadable;
    };
    let mut content = Vec::new();
    if file
        .by_ref()
        .take(parse::BINARY_SNIFF_BYTES as u64)
        .read_to_end(&mut content)
        .is_err()
    {
        return Untracked::Unreadable;
    }
    if parse::looks_binary(&content) {
        return Untracked::Binary;
    }
    // One byte more than the limit tells a file that fits from one that does not.
    let room = (read_limit + 1).saturating_sub(content.len()) as u64;
    if file.by_ref().take(room).read_to_end(&mut content).is_err() {
        return Untracked::Unreadable;
    }
    if content.len() > read_limit {
        Untracked::TooLarge
    } else {
        Untracked::Text(content)
    }
}
