//! Finds a Claude session's transcript on disk from its id, for sessions whose integration
//! never reported the file.
//!
//! Claude keeps each session in `<config dir>/projects/<project>/<session id>.jsonl`, where
//! the project folder is the session's starting directory with every character other than
//! an ASCII letter or digit replaced by `-`.

use std::path::{Path, PathBuf};

/// The transcript of Claude session `session_id`: first in the project folder of `cwd`,
/// then in any project folder, since the agent may have started elsewhere. Session ids
/// are UUIDs, so a match in another folder is still the session's own file. An id that
/// could name anything other than a single file is never looked up.
pub fn find_claude_transcript(
    config_dir: &Path,
    session_id: &str,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    let projects = config_dir.join("projects");
    let file_name = format!("{session_id}.jsonl");
    let expected = cwd.map(|cwd| projects.join(claude_project_folder(cwd)));
    if let Some(path) = expected
        .as_ref()
        .map(|folder| folder.join(&file_name))
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    std::fs::read_dir(&projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|folder| Some(folder) != expected.as_ref())
        .map(|folder| folder.join(&file_name))
        .find(|path| path.is_file())
}

fn claude_project_folder(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0b9c5c3e-6f1d-4c55-9d0e-2f3b8f1a7c42";

    fn config_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-transcript-locate-{test}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        dir
    }

    fn transcript_in(config_dir: &Path, folder: &str, session: &str) -> PathBuf {
        let folder = config_dir.join("projects").join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{session}.jsonl"));
        std::fs::write(&path, "{}\n").unwrap();
        path
    }

    #[test]
    fn finds_the_transcript_in_the_cwd_project_folder() {
        let dir = config_dir("cwd");
        let expected = transcript_in(&dir, "-Users-me-src-app-v1-2-my-repo", SESSION);
        transcript_in(&dir, "-elsewhere", SESSION);

        let found = find_claude_transcript(
            &dir,
            SESSION,
            Some(Path::new("/Users/me/src/app.v1/2_my repo")),
        );

        assert_eq!(found, Some(expected));
    }

    #[test]
    fn falls_back_to_any_project_folder() {
        let dir = config_dir("scan");
        transcript_in(&dir, "-Users-me-other", "another-session");
        let expected = transcript_in(&dir, "-Users-me-started-here", SESSION);

        let found = find_claude_transcript(&dir, SESSION, Some(Path::new("/Users/me/moved")));

        assert_eq!(found, Some(expected));
    }

    #[test]
    fn missing_transcript_is_not_found() {
        let dir = config_dir("missing");
        transcript_in(&dir, "-Users-me-app", "another-session");

        assert_eq!(
            find_claude_transcript(&dir, SESSION, Some(Path::new("/Users/me/app"))),
            None
        );
    }

    #[test]
    fn ids_that_could_escape_the_project_folders_are_refused() {
        let dir = config_dir("escape");
        std::fs::write(dir.join("secret.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(dir.join("projects").join("-Users-me-app")).unwrap();

        assert_eq!(
            find_claude_transcript(&dir, "../../secret", Some(Path::new("/Users/me/app"))),
            None
        );
    }
}
