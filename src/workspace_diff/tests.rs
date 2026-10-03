use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::api::schema::WorkspaceDiffFileStatus as Kind;

// ---- Pure parsing ------------------------------------------------------------------------

#[test]
fn name_status_reads_every_status_and_drops_a_cut_record() {
    let listed = parse::parse_name_status(
        b"M\0src/a.rs\0A\0added\0D\0gone\0T\0link\0X\0odd\0R087\0old.rs\0new.rs\0C100\0from\0copy\0",
    );
    let summary: Vec<_> = listed
        .iter()
        .map(|file| (file.status, file.path.as_str(), file.old_path.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            (Kind::Modified, "src/a.rs", None),
            (Kind::Added, "added", None),
            (Kind::Deleted, "gone", None),
            (Kind::TypeChanged, "link", None),
            (Kind::Unknown, "odd", None),
            (Kind::Renamed, "new.rs", Some("old.rs")),
            (Kind::Copied, "copy", Some("from")),
        ]
    );

    // Output that ends inside a record, as when Git was stopped, loses only that record.
    let cut = parse::parse_name_status(b"M\0a\0R087\0old.rs\0ne");
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0].path, "a");
    let cut = parse::parse_name_status(b"M\0a\0R087\0old.rs\0");
    assert_eq!(cut.len(), 1);
}

#[test]
fn numstat_reads_counts_binary_files_renames_and_tabs_in_paths() {
    let counted = parse::parse_numstat(
        b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x002\t0\t\0old.rs\0new.rs\x000\t0\tmode-only\x001\t1\ttab\tname\0",
    );
    let at = |path: &str| {
        counted
            .get(path)
            .copied()
            .unwrap_or_else(|| panic!("{path}"))
    };
    assert_eq!(at("src/a.rs").additions, Some(3));
    assert_eq!(at("src/a.rs").deletions, Some(1));
    assert!(at("logo.png").binary);
    assert_eq!(at("logo.png").additions, None);
    // A renamed file is found under the path it has now.
    assert_eq!(at("new.rs").additions, Some(2));
    assert!(!counted.contains_key("old.rs"));
    assert_eq!(at("mode-only").additions, Some(0));
    assert_eq!(at("tab\tname").deletions, Some(1));
}

#[test]
fn file_lists_leave_out_nested_repositories() {
    assert_eq!(
        parse::parse_file_list(b"a.txt\0nested/\0dir/b.txt\0"),
        ["a.txt", "dir/b.txt"]
    );
}

#[test]
fn headers_are_quoted_the_way_git_quotes_them() {
    assert_eq!(
        parse::diff_header("src/a b.rs", "src/a b.rs"),
        "diff --git a/src/a b.rs b/src/a b.rs"
    );
    assert_eq!(
        parse::diff_header("é/ü.txt", "é/ü.txt"),
        "diff --git a/é/ü.txt b/é/ü.txt"
    );
    assert_eq!(
        parse::diff_header("old\"q.txt", "new\ttab.txt"),
        "diff --git \"a/old\\\"q.txt\" \"b/new\\ttab.txt\""
    );
    assert_eq!(
        parse::diff_header("back\\slash", "back\\slash"),
        "diff --git \"a/back\\\\slash\" \"b/back\\\\slash\""
    );
    assert_eq!(
        parse::diff_header("bell\u{7}\u{7f}.txt", "bell\u{7}\u{7f}.txt"),
        "diff --git \"a/bell\\a\\177.txt\" \"b/bell\\a\\177.txt\""
    );
}

#[test]
fn patches_are_found_by_header_and_start_at_the_first_hunk() {
    let output = "\
diff --git a/a.txt b/a.txt
index 1..2 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-x
+y
diff --git a/b.txt b/b.txt
new file mode 100644
index 0..1
--- /dev/null
+++ b/b.txt
@@ -0,0 +1 @@
+z
\\ No newline at end of file
diff --git a/c.txt b/d.txt
similarity index 100%
rename from c.txt
rename to d.txt
diff --git a/mode b/mode
old mode 100644
new mode 100755
";
    let chunks = parse::split_patches(output, false);
    assert_eq!(
        chunks.patch("diff --git a/a.txt b/a.txt").as_deref(),
        Some("@@ -1 +1 @@\n-x\n+y\n")
    );
    assert_eq!(
        chunks.patch("diff --git a/b.txt b/b.txt").as_deref(),
        Some("@@ -0,0 +1 @@\n+z\n\\ No newline at end of file\n")
    );
    // No hunks: a rename that changed nothing, a change of mode.
    assert_eq!(
        chunks.patch("diff --git a/c.txt b/d.txt").as_deref(),
        Some("")
    );
    assert_eq!(
        chunks.patch("diff --git a/mode b/mode").as_deref(),
        Some("")
    );
    assert_eq!(chunks.patch("diff --git a/other b/other"), None);
}

#[test]
fn a_file_git_writes_in_two_parts_has_both() {
    let output = "\
diff --git a/thing b/thing
deleted file mode 100644
index 1..0
--- a/thing
+++ /dev/null
@@ -1 +0,0 @@
-was a file
diff --git a/thing b/thing
new file mode 120000
index 0..2
--- /dev/null
+++ b/thing
@@ -0,0 +1 @@
+target
\\ No newline at end of file
";
    let chunks = parse::split_patches(output, false);
    assert_eq!(
        chunks.patch("diff --git a/thing b/thing").as_deref(),
        Some("@@ -1 +0,0 @@\n-was a file\n@@ -0,0 +1 @@\n+target\n\\ No newline at end of file\n")
    );
}

#[test]
fn output_that_was_cut_loses_only_its_last_patch() {
    let output = "\
diff --git a/a b/a
@@ -1 +1 @@
-x
+y
diff --git a/b b/b
@@ -1 +1 @@
-p
+q
diff --git a/c b/c
@@ -1,3 +1,3 @@
-ha";
    let chunks = parse::split_patches(output, true);
    assert!(chunks.patch("diff --git a/a b/a").is_some());
    assert_eq!(
        chunks.patch("diff --git a/b b/b").as_deref(),
        Some("@@ -1 +1 @@\n-p\n+q\n")
    );
    assert_eq!(chunks.patch("diff --git a/c b/c"), None);

    // Without a cut the last patch counts, whatever it ends in.
    let whole = parse::split_patches(output, false);
    assert!(whole.patch("diff --git a/c b/c").is_some());
}

#[test]
fn a_new_file_patch_adds_every_line_and_marks_a_missing_final_newline() {
    assert_eq!(
        parse::added_file_patch(b"first\nsecond\n"),
        "@@ -0,0 +1,2 @@\n+first\n+second\n"
    );
    assert_eq!(
        parse::added_file_patch(b"only"),
        "@@ -0,0 +1,1 @@\n+only\n\\ No newline at end of file\n"
    );
    assert_eq!(parse::added_file_patch(b"\n"), "@@ -0,0 +1,1 @@\n+\n");
    assert_eq!(
        parse::added_file_patch(b"a\n\nb\r\n"),
        "@@ -0,0 +1,3 @@\n+a\n+\n+b\r\n"
    );
    assert_eq!(parse::added_file_patch(b""), "");
    assert_eq!(parse::count_added_lines(b"a\nb"), 2);
    assert_eq!(parse::count_added_lines(b"a\nb\n"), 2);
    assert_eq!(parse::count_added_lines(b""), 0);
}

#[test]
fn text_is_binary_when_a_nul_is_in_its_first_8_kib() {
    let mut content = vec![b'a'; 20_000];
    assert!(!parse::looks_binary(&content));
    content[parse::BINARY_SNIFF_BYTES - 1] = 0;
    assert!(parse::looks_binary(&content));
    content[parse::BINARY_SNIFF_BYTES - 1] = b'a';
    content[parse::BINARY_SNIFF_BYTES] = 0;
    assert!(!parse::looks_binary(&content));
}

#[test]
fn refs_are_shortened_the_way_people_write_them() {
    assert_eq!(parse::short_ref_name("refs/heads/main"), "main");
    assert_eq!(parse::short_ref_name("refs/heads/feature/x"), "feature/x");
    assert_eq!(
        parse::short_ref_name("refs/remotes/origin/main"),
        "origin/main"
    );
    assert_eq!(parse::short_ref_name("refs/tags/v1"), "v1");
    assert_eq!(
        parse::short_ref_name("refs/herdr/base/main"),
        "herdr/base/main"
    );
    assert_eq!(parse::short_ref_name("main"), "main");
}

#[test]
fn a_patch_too_large_alone_costs_the_reply_nothing_but_one_too_large_for_it_ends_the_patches() {
    let mut budget = PatchBudget::new(false);
    assert!(budget.admit(0));
    assert!(!budget.admit(FILE_PATCH_LIMIT + 1));
    assert!(budget.admit(FILE_PATCH_LIMIT));
    assert!(budget.admit(FILE_PATCH_LIMIT));
    assert!(budget.admit(FILE_PATCH_LIMIT));
    assert!(budget.admit(FILE_PATCH_LIMIT - 10));
    // 10 bytes are left, and a patch that does not fit ends the patches, even a tiny one
    // that would have.
    assert!(!budget.admit(11));
    assert!(!budget.admit(1));
    assert!(budget.admit(0));
}

// ---- Temporary repositories --------------------------------------------------------------

struct Repo {
    path: PathBuf,
}

impl Repo {
    fn init(name: &str) -> Self {
        Self::init_on(name, "main")
    }

    fn init_on(name: &str, branch: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-workspace-diff-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self { path };
        repo.git(&["init", "--quiet", "-b", branch]);
        repo.git(&["config", "user.email", "herdr@example.invalid"]);
        repo.git(&["config", "user.name", "Herdr Test"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        let path = self.path.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn remove(&self, relative: &str) {
        std::fs::remove_file(self.path.join(relative)).unwrap();
    }

    fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "--no-verify", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn diff(&self) -> WorkspaceDiffInfo {
        self.diff_with(DiffRequest::default())
    }

    fn diff_with(&self, request: DiffRequest) -> WorkspaceDiffInfo {
        read_workspace_diff("w1", &self.path, &request)
    }

    fn summary(&self) -> WorkspaceDiffInfo {
        self.diff_with(DiffRequest {
            path: None,
            summary_only: true,
        })
    }

    fn only(&self, path: &str) -> WorkspaceDiffInfo {
        self.diff_with(DiffRequest {
            path: Some(path.into()),
            summary_only: false,
        })
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn paths(diff: &WorkspaceDiffInfo) -> Vec<&str> {
    diff.files.iter().map(|file| file.path.as_str()).collect()
}

fn file<'a>(diff: &'a WorkspaceDiffInfo, path: &str) -> &'a WorkspaceDiffFile {
    diff.files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("{path} is not in {:?}", paths(diff)))
}

fn base_of(diff: &WorkspaceDiffInfo) -> Option<(String, String)> {
    diff.base
        .as_ref()
        .map(|base| (base.ref_name.clone().unwrap(), base.commit.clone()))
}

/// `count` lines of 79 characters.
fn lines(tag: &str, count: usize) -> String {
    (0..count)
        .map(|i| format!("{tag}{i:05}{}\n", "x".repeat(79 - tag.len() - 5)))
        .collect()
}

// ---- What is listed ----------------------------------------------------------------------

#[test]
fn lists_every_kind_of_change_with_its_counts_and_patch() {
    let repo = Repo::init("kinds");
    repo.write("a.txt", "one\ntwo\nthree\n");
    repo.write("c.txt", "gone\n");
    repo.write("old_name.rs", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n");
    repo.write("keep.txt", "keep\n");
    let head = repo.commit_all("initial");

    repo.write("a.txt", "one\nTWO\nthree\nfour\n");
    repo.remove("c.txt");
    repo.git(&["mv", "old_name.rs", "new_name.rs"]);
    repo.write("new_name.rs", "l1\nl2\nL3\nl4\nl5\nl6\nl7\nl8\n");
    repo.write("added.txt", "x\ny\n");
    repo.git(&["add", "added.txt"]);
    repo.write("logo.png", [0x89u8, b'P', b'N', b'G', 0, 1, 2]);
    repo.git(&["add", "logo.png"]);
    repo.write("notes.md", "first\nsecond\n");
    repo.write("data.bin", [0u8, 1, 2, 3]);

    let diff = repo.diff();

    assert_eq!(diff.status, WorkspaceDiffStatus::Available);
    assert_eq!(diff.workspace_id, "w1");
    assert_eq!(
        PathBuf::from(diff.directory.as_deref().unwrap()),
        std::fs::canonicalize(&repo.path).unwrap()
    );
    assert_eq!(diff.branch.as_deref(), Some("main"));
    assert_eq!(diff.head.as_deref(), Some(head.as_str()));
    assert_eq!(base_of(&diff), Some(("main".into(), head.clone())));
    assert!(!diff.truncated);
    assert_eq!(
        paths(&diff),
        [
            "a.txt",
            "added.txt",
            "c.txt",
            "data.bin",
            "logo.png",
            "new_name.rs",
            "notes.md"
        ]
    );

    let modified = file(&diff, "a.txt");
    assert_eq!(modified.status, Kind::Modified);
    assert_eq!((modified.additions, modified.deletions), (Some(2), Some(1)));
    let patch = modified.patch.as_deref().unwrap();
    assert!(patch.starts_with("@@ -1,3 +1,4 @@\n"), "{patch}");
    assert!(patch.contains("-two\n+TWO\n"), "{patch}");
    assert!(!modified.patch_truncated && !modified.binary);

    let added = file(&diff, "added.txt");
    assert_eq!(added.status, Kind::Added);
    assert_eq!((added.additions, added.deletions), (Some(2), Some(0)));
    assert_eq!(added.patch.as_deref(), Some("@@ -0,0 +1,2 @@\n+x\n+y\n"));

    let deleted = file(&diff, "c.txt");
    assert_eq!(deleted.status, Kind::Deleted);
    assert_eq!((deleted.additions, deleted.deletions), (Some(0), Some(1)));
    assert_eq!(deleted.patch.as_deref(), Some("@@ -1 +0,0 @@\n-gone\n"));

    let renamed = file(&diff, "new_name.rs");
    assert_eq!(renamed.status, Kind::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("old_name.rs"));
    assert_eq!((renamed.additions, renamed.deletions), (Some(1), Some(1)));
    assert!(renamed.patch.as_deref().unwrap().starts_with("@@ "));

    let untracked = file(&diff, "notes.md");
    assert_eq!(untracked.status, Kind::Untracked);
    assert_eq!(
        (untracked.additions, untracked.deletions),
        (Some(2), Some(0))
    );
    assert_eq!(
        untracked.patch.as_deref(),
        Some("@@ -0,0 +1,2 @@\n+first\n+second\n")
    );

    for binary in ["logo.png", "data.bin"] {
        let binary = file(&diff, binary);
        assert!(binary.binary);
        assert_eq!((binary.additions, binary.deletions), (None, None));
        assert_eq!(binary.patch, None);
        assert!(!binary.patch_truncated);
    }
    assert_eq!(file(&diff, "logo.png").status, Kind::Added);
    assert_eq!(file(&diff, "data.bin").status, Kind::Untracked);

    // Counts of text files only, and only of files that are listed.
    assert_eq!(diff.additions, 2 + 2 + 1 + 2);
    assert_eq!(diff.deletions, 1 + 1 + 1);
    // Every text file has a patch that starts at its first hunk.
    for file in diff.files.iter().filter(|file| !file.binary) {
        let patch = file
            .patch
            .as_deref()
            .unwrap_or_else(|| panic!("{}", file.path));
        assert!(patch.starts_with("@@ "), "{}: {patch}", file.path);
    }
}

#[test]
fn a_file_edited_after_it_was_staged_is_one_change_against_the_base() {
    let repo = Repo::init("staged-and-unstaged");
    repo.write("a.txt", "v1\n");
    repo.commit_all("initial");
    repo.write("a.txt", "v2\n");
    repo.git(&["add", "a.txt"]);
    repo.write("a.txt", "v3\n");

    let diff = repo.diff();

    assert_eq!(paths(&diff), ["a.txt"]);
    assert_eq!(
        diff.files[0].patch.as_deref(),
        Some("@@ -1 +1 @@\n-v1\n+v3\n")
    );
}

#[test]
fn a_renamed_file_that_did_not_change_has_no_hunks_but_a_patch() {
    let repo = Repo::init("pure-rename");
    repo.write("a.txt", "same\ncontent\n");
    repo.commit_all("initial");
    repo.git(&["mv", "a.txt", "b.txt"]);

    let diff = repo.diff();

    let renamed = file(&diff, "b.txt");
    assert_eq!(renamed.status, Kind::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("a.txt"));
    assert_eq!((renamed.additions, renamed.deletions), (Some(0), Some(0)));
    assert_eq!(renamed.patch.as_deref(), Some(""));
    assert!(!renamed.patch_truncated);
}

#[test]
fn ignored_files_and_nested_repositories_are_not_listed() {
    let repo = Repo::init("ignored");
    repo.write(".gitignore", "*.log\n");
    repo.commit_all("initial");
    repo.write("build.log", "noise\n");
    repo.write("kept.txt", "kept\n");
    std::fs::create_dir_all(repo.path.join("nested")).unwrap();
    let nested = Command::new("git")
        .arg("-C")
        .arg(repo.path.join("nested"))
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(nested.success());
    repo.write("nested/inner.txt", "inner\n");

    assert_eq!(paths(&repo.diff()), ["kept.txt"]);
}

#[cfg(unix)]
#[test]
fn a_mode_change_is_a_change_without_hunks() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::init("mode");
    repo.write("run.sh", "echo hi\n");
    repo.commit_all("initial");
    std::fs::set_permissions(
        repo.path.join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let diff = repo.diff();

    let file = file(&diff, "run.sh");
    assert_eq!(file.status, Kind::Modified);
    assert_eq!((file.additions, file.deletions), (Some(0), Some(0)));
    assert_eq!(file.patch.as_deref(), Some(""));
}

#[cfg(unix)]
#[test]
fn a_file_turned_into_a_symlink_is_a_type_change_with_both_halves() {
    let repo = Repo::init("typechange");
    repo.write("thing", "was a file\n");
    repo.commit_all("initial");
    repo.remove("thing");
    std::os::unix::fs::symlink("target.txt", repo.path.join("thing")).unwrap();

    let diff = repo.diff();

    let thing = file(&diff, "thing");
    assert_eq!(thing.status, Kind::TypeChanged);
    let patch = thing.patch.as_deref().unwrap();
    assert!(patch.contains("-was a file\n"), "{patch}");
    assert!(patch.contains("+target.txt\n"), "{patch}");
}

#[cfg(unix)]
#[test]
fn an_untracked_symlink_is_shown_as_the_path_it_points_to_and_never_followed() {
    let repo = Repo::init("untracked-symlink");
    repo.write("seed.txt", "seed\n");
    repo.commit_all("initial");
    let outside = std::env::temp_dir().join(format!("herdr-diff-outside-{}", std::process::id()));
    std::fs::write(&outside, "secret contents\n").unwrap();
    std::os::unix::fs::symlink(&outside, repo.path.join("link")).unwrap();

    let diff = repo.diff();

    let link = file(&diff, "link");
    assert_eq!(link.status, Kind::Untracked);
    let expected = format!(
        "@@ -0,0 +1,1 @@\n+{}\n\\ No newline at end of file\n",
        outside.display()
    );
    assert_eq!(link.patch.as_deref(), Some(expected.as_str()));
    assert_eq!(link.additions, Some(1));
    let _ = std::fs::remove_file(outside);
}

#[cfg(unix)]
#[test]
fn files_with_unusual_names_still_get_their_patches() {
    let repo = Repo::init("names");
    let names = [
        "with space.txt",
        "dir/sub dir/file.txt",
        "é-ü.txt",
        "quo\"te.txt",
        "tab\tname.txt",
        "back\\slash.txt",
        "new\nline.txt",
        "bell\u{7}.txt",
    ];
    for name in names {
        repo.write(name, "before\n");
    }
    repo.commit_all("initial");
    for name in names {
        repo.write(name, "after\n");
    }
    repo.write("fresh \"one\".txt", "new\n");

    let diff = repo.diff();

    assert_eq!(diff.files.len(), names.len() + 1);
    for name in names {
        let changed = file(&diff, name);
        assert_eq!(changed.status, Kind::Modified, "{name:?}");
        assert_eq!(
            changed.patch.as_deref(),
            Some("@@ -1 +1 @@\n-before\n+after\n"),
            "{name:?}"
        );
    }
    assert_eq!(
        file(&diff, "fresh \"one\".txt").patch.as_deref(),
        Some("@@ -0,0 +1,1 @@\n+new\n")
    );
}

// ---- The base ----------------------------------------------------------------------------

#[test]
fn shows_what_the_branch_changed_since_the_fork_and_not_what_the_base_gained() {
    let repo = Repo::init("fork");
    repo.write("shared.txt", "shared\n");
    let fork_point = repo.commit_all("initial");
    repo.git(&["checkout", "--quiet", "-b", "feature"]);
    repo.write("feature.txt", "feature work\n");
    repo.commit_all("feature work");
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("main-only.txt", "landed on main\n");
    repo.write("shared.txt", "shared, edited on main\n");
    repo.commit_all("main moves on");
    repo.git(&["checkout", "--quiet", "feature"]);
    repo.write("scratch.txt", "uncommitted\n");

    let diff = repo.diff();

    assert_eq!(diff.branch.as_deref(), Some("feature"));
    assert_eq!(base_of(&diff), Some(("main".into(), fork_point)));
    // The committed work and the uncommitted file, and nothing main gained after the fork.
    assert_eq!(paths(&diff), ["feature.txt", "scratch.txt"]);
    assert_eq!(file(&diff, "feature.txt").status, Kind::Added);
    assert_eq!(file(&diff, "scratch.txt").status, Kind::Untracked);
}

#[test]
fn picks_the_recorded_base_then_the_remotes_default_then_main_then_master() {
    let repo = Repo::init("precedence");
    repo.write("f", "0\n");
    let m0 = repo.commit_all("m0");
    repo.git(&["branch", "old"]);
    repo.write("f", "1\n");
    let m1 = repo.commit_all("m1");
    repo.git(&["checkout", "--quiet", "-b", "feature"]);
    repo.write("g", "feature\n");
    repo.commit_all("f1");
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("f", "2\n");
    repo.commit_all("m2");
    repo.git(&["update-ref", "refs/remotes/origin/trunk", &m1]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    repo.git(&["checkout", "--quiet", "feature"]);

    // The base recorded for the branch beats everything.
    repo.git(&["config", "branch.feature.herdrBase", "refs/heads/old"]);
    assert_eq!(base_of(&repo.diff()), Some(("old".into(), m0.clone())));

    // A recorded base that is gone gives way to the next.
    repo.git(&["config", "branch.feature.herdrBase", "refs/heads/deleted"]);
    assert_eq!(
        base_of(&repo.diff()),
        Some(("origin/trunk".into(), m1.clone()))
    );
    repo.git(&["config", "--unset", "branch.feature.herdrBase"]);

    // The default branch of the remote, which is not `main`.
    assert_eq!(
        base_of(&repo.diff()),
        Some(("origin/trunk".into(), m1.clone()))
    );

    // The branch's own remote beats `origin`.
    repo.git(&["update-ref", "refs/remotes/upstream/dev", &m0]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/upstream/HEAD",
        "refs/remotes/upstream/dev",
    ]);
    repo.git(&["config", "branch.feature.remote", "upstream"]);
    assert_eq!(base_of(&repo.diff()), Some(("upstream/dev".into(), m0)));
    repo.git(&["config", "--unset", "branch.feature.remote"]);

    // Without a remote default: local `main`, whose merge-base is the fork point.
    repo.git(&["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
    assert_eq!(base_of(&repo.diff()), Some(("main".into(), m1.clone())));

    // Then `master`.
    repo.git(&["branch", "-m", "main", "master"]);
    assert_eq!(base_of(&repo.diff()), Some(("master".into(), m1)));

    // Then nothing.
    repo.git(&["branch", "-D", "master"]);
    assert_eq!(base_of(&repo.diff()), None);
}

#[test]
fn a_detached_head_has_no_branch_and_still_finds_its_base() {
    let repo = Repo::init("detached");
    repo.write("a.txt", "a\n");
    let first = repo.commit_all("initial");
    repo.git(&["checkout", "--quiet", "--detach"]);
    repo.write("b.txt", "b\n");
    let detached = repo.commit_all("detached work");

    let diff = repo.diff();

    assert_eq!(diff.branch, None);
    assert_eq!(diff.head.as_deref(), Some(detached.as_str()));
    assert_eq!(base_of(&diff), Some(("main".into(), first)));
    assert_eq!(paths(&diff), ["b.txt"]);
}

#[test]
fn without_a_base_branch_only_the_uncommitted_changes_show() {
    let repo = Repo::init_on("nobase", "work");
    repo.write("one.txt", "1\n");
    repo.commit_all("one");
    repo.write("two.txt", "2\n");
    let head = repo.commit_all("two");
    repo.write("one.txt", "1\nchanged\n");
    repo.write("loose.txt", "new\n");

    let diff = repo.diff();

    assert_eq!(diff.status, WorkspaceDiffStatus::Available);
    assert_eq!(diff.base, None);
    assert_eq!(diff.head.as_deref(), Some(head.as_str()));
    assert_eq!(diff.branch.as_deref(), Some("work"));
    // `two.txt` was committed, and there is nothing to compare the commits with.
    assert_eq!(paths(&diff), ["loose.txt", "one.txt"]);
    assert_eq!(file(&diff, "one.txt").status, Kind::Modified);
}

#[test]
fn a_repository_without_commits_lists_what_is_staged_and_untracked() {
    let repo = Repo::init("unborn");
    repo.write("staged.txt", "s\n");
    repo.git(&["add", "staged.txt"]);
    repo.write("loose.txt", "l\n");

    let diff = repo.diff();

    assert_eq!(diff.status, WorkspaceDiffStatus::Available);
    assert_eq!(diff.branch.as_deref(), Some("main"));
    assert_eq!(diff.head, None);
    assert_eq!(diff.base, None);
    assert_eq!(paths(&diff), ["loose.txt", "staged.txt"]);
    let staged = file(&diff, "staged.txt");
    assert_eq!(staged.status, Kind::Added);
    assert_eq!(staged.patch.as_deref(), Some("@@ -0,0 +1 @@\n+s\n"));
    assert_eq!(file(&diff, "loose.txt").status, Kind::Untracked);
}

// ---- When there is nothing to read ------------------------------------------------------

#[test]
fn a_directory_outside_any_repository_is_not_a_repository() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-workspace-diff-norepo-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let diff = read_workspace_diff("w1", &dir, &DiffRequest::default());

    assert_eq!(diff.status, WorkspaceDiffStatus::NotARepository);
    assert_eq!(
        diff.directory.as_deref(),
        Some(dir.display().to_string().as_str())
    );
    assert!(diff.files.is_empty());
    assert_eq!(diff.error, None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_git_directory_is_not_a_work_tree() {
    let repo = Repo::init("gitdir");
    repo.write("a.txt", "a\n");
    repo.commit_all("initial");

    let diff = read_workspace_diff("w1", &repo.path.join(".git"), &DiffRequest::default());

    assert_eq!(diff.status, WorkspaceDiffStatus::NotARepository);
}

#[test]
fn a_directory_that_is_gone_has_no_directory() {
    let missing = std::env::temp_dir().join(format!(
        "herdr-workspace-diff-missing-{}",
        std::process::id()
    ));

    let diff = read_workspace_diff("w9", &missing, &DiffRequest::default());

    assert_eq!(diff.status, WorkspaceDiffStatus::NoDirectory);
    assert_eq!(diff.workspace_id, "w9");
    assert!(diff.files.is_empty());
    assert_eq!(no_directory("w9").directory, None);
    assert_eq!(no_directory("w9").status, WorkspaceDiffStatus::NoDirectory);
}

#[test]
fn a_failing_git_is_unreadable_and_says_why() {
    let repo = Repo::init("corrupt");
    repo.write("a.txt", "a\n");
    repo.commit_all("initial");
    repo.write("a.txt", "changed\n");
    std::fs::write(repo.path.join(".git/index"), "this is not an index").unwrap();

    let diff = repo.diff();

    assert_eq!(diff.status, WorkspaceDiffStatus::Unreadable);
    let error = diff.error.as_deref().unwrap();
    assert!(error.starts_with("fatal:"), "{error}");
    // Git's own wording for a broken index differs between versions; it names the index.
    assert!(error.contains("index"), "{error}");
    assert!(diff.files.is_empty());
}

// ---- Budgets and options -----------------------------------------------------------------

#[test]
fn a_patch_over_the_file_limit_comes_without_and_leaves_the_reply_budget_alone() {
    let repo = Repo::init("file-limit");
    repo.write("big.txt", lines("old", 3000));
    repo.write("small.txt", "a\n");
    repo.commit_all("initial");
    repo.write("big.txt", lines("new", 3000));
    repo.write("small.txt", "a\nb\n");

    let diff = repo.diff();

    let big = file(&diff, "big.txt");
    assert_eq!(big.patch, None);
    assert!(big.patch_truncated);
    assert!(!big.binary);
    // Its counts are still exact, and it did not keep the next file from its patch.
    assert_eq!((big.additions, big.deletions), (Some(3000), Some(3000)));
    assert_eq!(
        file(&diff, "small.txt").patch.as_deref(),
        Some("@@ -1 +1,2 @@\n a\n+b\n")
    );
    assert_eq!(diff.additions, 3001);
}

#[test]
fn patches_end_once_the_reply_budget_is_used_and_the_files_after_come_without() {
    let repo = Repo::init("reply-budget");
    repo.write("seed.txt", "seed\n");
    repo.commit_all("initial");
    // Each patch is 105,319 bytes: four fit in 512 KiB, a fifth does not.
    for number in 1..=5 {
        repo.write(&format!("big{number}.txt"), lines("n", 1300));
    }
    repo.write("tiny.txt", "tiny\n");

    let diff = repo.diff();

    assert_eq!(
        paths(&diff),
        ["big1.txt", "big2.txt", "big3.txt", "big4.txt", "big5.txt", "tiny.txt"]
    );
    for name in ["big1.txt", "big2.txt", "big3.txt", "big4.txt"] {
        let patch = file(&diff, name).patch.as_deref().unwrap();
        assert_eq!(patch.len(), 105_319, "{name}");
    }
    for name in ["big5.txt", "tiny.txt"] {
        let without = file(&diff, name);
        assert_eq!(without.patch, None, "{name}");
        assert!(without.patch_truncated, "{name}");
        assert!(without.additions.is_some(), "{name}");
    }
    assert_eq!(diff.additions, 5 * 1300 + 1);
}

#[test]
fn a_summary_has_the_counts_and_no_patches() {
    let repo = Repo::init("summary");
    repo.write("a.txt", "one\ntwo\n");
    repo.write("gone.txt", "bye\n");
    repo.write("big.txt", lines("old", 3000));
    repo.commit_all("initial");
    repo.write("a.txt", "one\ntwo\nthree\n");
    repo.remove("gone.txt");
    repo.write("big.txt", lines("new", 3000));
    repo.write("fresh.txt", "x\ny\nz\n");
    repo.write("blob.bin", [0u8, 1]);

    let diff = repo.summary();

    assert_eq!(
        paths(&diff),
        ["a.txt", "big.txt", "blob.bin", "fresh.txt", "gone.txt"]
    );
    for file in &diff.files {
        assert_eq!(file.patch, None, "{}", file.path);
        assert!(!file.patch_truncated, "{}", file.path);
    }
    assert_eq!(file(&diff, "fresh.txt").additions, Some(3));
    assert_eq!(file(&diff, "big.txt").additions, Some(3000));
    assert!(file(&diff, "blob.bin").binary);
    assert_eq!(diff.additions, 1 + 3000 + 3);
    assert_eq!(diff.deletions, 3000 + 1);
}

#[test]
fn a_path_limits_the_reply_to_that_file_and_allows_a_larger_patch() {
    let repo = Repo::init("single-file");
    repo.write("big.txt", lines("old", 3000));
    repo.write("moved.txt", "a\nb\nc\nd\ne\nf\n");
    repo.write("other.txt", "o\n");
    repo.commit_all("initial");
    repo.write("big.txt", lines("new", 3000));
    repo.git(&["mv", "moved.txt", "renamed.txt"]);
    repo.write("other.txt", "o\nchanged\n");
    repo.write("untracked.txt", "u\n");

    // Over the allowance of a whole reply, but not of one file asked for.
    let big = repo.only("big.txt");
    assert_eq!(paths(&big), ["big.txt"]);
    let patch = big.files[0].patch.as_deref().unwrap();
    assert!(patch.starts_with("@@ -1,3000 +1,3000 @@\n"));
    assert!(patch.len() > FILE_PATCH_LIMIT);
    assert!(!big.files[0].patch_truncated);
    // Totals count what is listed.
    assert_eq!((big.additions, big.deletions), (3000, 3000));

    let renamed_by_its_old_name = repo.only("moved.txt");
    assert_eq!(paths(&renamed_by_its_old_name), ["renamed.txt"]);
    assert_eq!(
        renamed_by_its_old_name.files[0].old_path.as_deref(),
        Some("moved.txt")
    );
    assert_eq!(repo.only("renamed.txt").files.len(), 1);

    let untracked = repo.only("untracked.txt");
    assert_eq!(paths(&untracked), ["untracked.txt"]);
    assert_eq!(
        untracked.files[0].patch.as_deref(),
        Some("@@ -0,0 +1,1 @@\n+u\n")
    );

    let unknown = repo.only("nope.txt");
    assert_eq!(unknown.status, WorkspaceDiffStatus::Available);
    assert!(unknown.files.is_empty());

    // Without it, the same file is too large.
    let everything = repo.diff();
    assert_eq!(file(&everything, "big.txt").patch, None);
    assert!(file(&everything, "big.txt").patch_truncated);
}

#[test]
fn a_path_is_taken_literally_and_not_as_a_pattern() {
    let repo = Repo::init("literal-path");
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.commit_all("initial");
    repo.write("a.txt", "a2\n");
    repo.write("b.txt", "b2\n");

    assert!(repo.only("*.txt").files.is_empty());
    assert_eq!(paths(&repo.only("a.txt")), ["a.txt"]);
}

#[test]
fn only_the_first_two_thousand_files_are_listed() {
    let repo = Repo::init("file-cap");
    repo.write("seed.txt", "seed\n");
    repo.commit_all("initial");
    for number in 0..=MAX_FILES {
        repo.write(&format!("many/f{number:04}.txt"), "x\n");
    }

    let diff = repo.summary();

    assert_eq!(diff.files.len(), MAX_FILES);
    assert!(diff.truncated);
    assert_eq!(diff.files[0].path, "many/f0000.txt");
    assert_eq!(diff.files[MAX_FILES - 1].path, "many/f1999.txt");
    // Totals count what is listed.
    assert_eq!(diff.additions, MAX_FILES as u64);

    let exactly = Repo::init("file-cap-exact");
    exactly.write("seed.txt", "seed\n");
    exactly.commit_all("initial");
    for number in 0..MAX_FILES {
        exactly.write(&format!("many/f{number:04}.txt"), "x\n");
    }
    let exact = exactly.summary();
    assert_eq!(exact.files.len(), MAX_FILES);
    assert!(!exact.truncated);
}

#[test]
fn an_untracked_file_over_the_read_limit_has_no_patch_and_no_counts_until_asked_for_alone() {
    let repo = Repo::init("untracked-large");
    repo.write("seed.txt", "seed\n");
    repo.commit_all("initial");
    repo.write("large.log", lines("l", 3600));
    repo.write("fits.log", lines("f", 1000));

    let diff = repo.diff();

    let large = file(&diff, "large.log");
    assert_eq!(large.status, Kind::Untracked);
    assert_eq!(large.patch, None);
    assert!(large.patch_truncated);
    assert!(!large.binary);
    assert_eq!((large.additions, large.deletions), (None, None));
    assert_eq!(file(&diff, "fits.log").additions, Some(1000));
    assert_eq!(diff.additions, 1000);

    let alone = repo.only("large.log");
    let large = &alone.files[0];
    assert_eq!(large.additions, Some(3600));
    assert!(large
        .patch
        .as_deref()
        .unwrap()
        .starts_with("@@ -0,0 +1,3600 @@\n"));
    assert!(!large.patch_truncated);
}

#[test]
fn a_change_far_larger_than_what_is_kept_still_answers_and_cuts_the_patches_after_it() {
    let repo = Repo::init("huge");
    repo.write("a-big.txt", "one line\n");
    repo.write("z-small.txt", "a\n");
    repo.commit_all("initial");
    // About 13 MB of patch, more than the output that is kept.
    repo.write("a-big.txt", format!("{}\n", "x".repeat(79)).repeat(160_000));
    repo.write("z-small.txt", "a\nb\n");

    let diff = repo.diff();

    assert_eq!(diff.status, WorkspaceDiffStatus::Available);
    let big = file(&diff, "a-big.txt");
    assert_eq!((big.additions, big.deletions), (Some(160_000), Some(1)));
    assert_eq!(big.patch, None);
    assert!(big.patch_truncated);
    // Its patch lies beyond what was kept, so it cannot be told.
    let small = file(&diff, "z-small.txt");
    assert_eq!(small.additions, Some(1));
    assert_eq!(small.patch, None);
    assert!(small.patch_truncated);
    assert_eq!(diff.additions, 160_001);
}
