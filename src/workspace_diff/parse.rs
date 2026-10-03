//! The parts of `workspace.diff` that need no Git or filesystem: reading Git's machine
//! output and writing the patch of a file Git does not know yet.

use std::collections::HashMap;

use crate::api::schema::WorkspaceDiffFileStatus;

/// How much of a file Git looks at to decide that it is binary.
pub(super) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// A changed file as `git diff --name-status -z` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub status: WorkspaceDiffFileStatus,
    pub path: String,
    pub old_path: Option<String>,
}

/// The line counts of a file as `git diff --numstat -z` gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counted {
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub binary: bool,
}

/// The NUL-terminated tokens of `bytes`. A last token without its NUL, which is what a cut
/// off output ends in, is dropped.
fn nul_tokens(bytes: &[u8]) -> Vec<&[u8]> {
    let mut tokens: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    tokens.pop();
    tokens
}

/// Paths with invalid UTF-8 become U+FFFD. The same conversion is applied to Git's patch
/// headers, so such a file's patch is still found, but the file itself no longer is.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(super) fn parse_name_status(bytes: &[u8]) -> Vec<Listed> {
    let mut tokens = nul_tokens(bytes).into_iter();
    let mut listed = Vec::new();
    while let Some(code) = tokens.next() {
        let status = match code.first() {
            Some(b'A') => WorkspaceDiffFileStatus::Added,
            Some(b'M') => WorkspaceDiffFileStatus::Modified,
            Some(b'D') => WorkspaceDiffFileStatus::Deleted,
            Some(b'R') => WorkspaceDiffFileStatus::Renamed,
            Some(b'C') => WorkspaceDiffFileStatus::Copied,
            Some(b'T') => WorkspaceDiffFileStatus::TypeChanged,
            _ => WorkspaceDiffFileStatus::Unknown,
        };
        let Some(first) = tokens.next() else { break };
        let (old_path, path) = if matches!(
            status,
            WorkspaceDiffFileStatus::Renamed | WorkspaceDiffFileStatus::Copied
        ) {
            let Some(second) = tokens.next() else { break };
            (Some(lossy(first)), lossy(second))
        } else {
            (None, lossy(first))
        };
        listed.push(Listed {
            status,
            path,
            old_path,
        });
    }
    listed
}

/// Line counts by the path a file has now.
pub(super) fn parse_numstat(bytes: &[u8]) -> HashMap<String, Counted> {
    let mut tokens = nul_tokens(bytes).into_iter();
    let mut counted = HashMap::new();
    while let Some(token) = tokens.next() {
        let record = lossy(token);
        let mut fields = record.splitn(3, '\t');
        let (Some(additions), Some(deletions), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A renamed or copied file has no path in its record; the old and the new path
        // follow as tokens of their own.
        let path = if path.is_empty() {
            let (Some(_old), Some(new)) = (tokens.next(), tokens.next()) else {
                break;
            };
            lossy(new)
        } else {
            path.to_string()
        };
        let counts = if additions == "-" && deletions == "-" {
            Counted {
                additions: None,
                deletions: None,
                binary: true,
            }
        } else {
            Counted {
                additions: additions.parse().ok(),
                deletions: deletions.parse().ok(),
                binary: false,
            }
        };
        counted.insert(path, counts);
    }
    counted
}

/// The files of `git ls-files -z`. Directories, which Git lists for a repository nested
/// in the checkout, are left out.
pub(super) fn parse_file_list(bytes: &[u8]) -> Vec<String> {
    nul_tokens(bytes)
        .into_iter()
        .filter(|token| !token.is_empty() && !token.ends_with(b"/"))
        .map(lossy)
        .collect()
}

fn needs_quoting(c: char) -> bool {
    c < ' ' || c == '\u{7f}' || c == '"' || c == '\\'
}

/// A path as Git writes it in a patch header with `core.quotepath=off`: bare, unless it
/// holds a control character, a quote or a backslash.
fn quote_path(path: &str) -> String {
    if !path.chars().any(needs_quoting) {
        return path.to_string();
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for c in path.chars() {
        match c {
            '\u{7}' => quoted.push_str("\\a"),
            '\u{8}' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\u{b}' => quoted.push_str("\\v"),
            '\u{c}' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if needs_quoting(c) => quoted.push_str(&format!("\\{:03o}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The `diff --git` line Git starts the patch of a file with, given `--src-prefix=a/` and
/// `--dst-prefix=b/`.
pub(super) fn diff_header(old_path: &str, path: &str) -> String {
    format!(
        "diff --git {} {}",
        quote_path(&format!("a/{old_path}")),
        quote_path(&format!("b/{path}"))
    )
}

/// The patches of one `git diff` output, found by the `diff --git` line each starts with.
pub(super) struct PatchChunks<'a> {
    by_header: HashMap<&'a str, Vec<&'a str>>,
}

impl PatchChunks<'_> {
    /// The hunks of the file whose patch starts with `header`, from its first `@@` line;
    /// empty when it has none, as for a renamed file that did not change. A file Git
    /// writes in two parts, one that removes what the file was and one that adds what it
    /// is now, as when it turns into a symbolic link, has both. `None` when the output has
    /// no patch for it.
    pub(super) fn patch(&self, header: &str) -> Option<String> {
        self.by_header.get(header).map(|parts| parts.concat())
    }
}

/// Splits the output of `git diff` into the patches of its files. With `cut`, the output
/// ended early, and its last patch, which may be incomplete, is left out.
pub(super) fn split_patches(output: &str, cut: bool) -> PatchChunks<'_> {
    // No line of a patch body starts with `diff --git`: they start with a space, `+`,
    // `-` or a backslash.
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in output.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    let mut spans: Vec<(usize, usize)> = starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            (
                start,
                starts.get(index + 1).copied().unwrap_or(output.len()),
            )
        })
        .collect();
    if cut {
        spans.pop();
    }
    let mut by_header: HashMap<&str, Vec<&str>> = HashMap::new();
    for (start, end) in spans {
        let chunk = &output[start..end];
        let header_end = chunk.find('\n').unwrap_or(chunk.len());
        let header = &chunk[..header_end];
        let mut position = (header_end + 1).min(chunk.len());
        let mut hunks = "";
        for line in chunk[position..].split_inclusive('\n') {
            if line.starts_with("@@ ") {
                hunks = &chunk[position..];
                break;
            }
            position += line.len();
        }
        by_header.entry(header).or_default().push(hunks);
    }
    PatchChunks { by_header }
}

/// Whether `content`, the start of a file, looks binary the way Git decides: a NUL byte in
/// its first 8 KiB.
pub(super) fn looks_binary(content: &[u8]) -> bool {
    content[..content.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// The lines a file of `content` adds: one more than its newlines when it does not end in
/// one, none when it is empty.
pub(super) fn count_added_lines(content: &[u8]) -> u64 {
    let newlines = content.iter().filter(|byte| **byte == b'\n').count() as u64;
    match content.last() {
        None => 0,
        Some(b'\n') => newlines,
        Some(_) => newlines + 1,
    }
}

/// The patch Git would write for a new file of `content`: the hunk that adds all of it.
/// Empty for an empty file, which has no hunk.
pub(super) fn added_file_patch(content: &[u8]) -> String {
    let lines = count_added_lines(content);
    if lines == 0 {
        return String::new();
    }
    let text = String::from_utf8_lossy(content);
    let ends_with_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let mut patch = format!("@@ -0,0 +1,{lines} @@\n");
    for line in body.split('\n') {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !ends_with_newline {
        patch.push_str("\\ No newline at end of file\n");
    }
    patch
}

/// A ref the way people write it: `refs/heads/main` as `main`.
pub(super) fn short_ref_name(full: &str) -> &str {
    for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/", "refs/"] {
        if let Some(short) = full.strip_prefix(prefix) {
            return short;
        }
    }
    full
}
