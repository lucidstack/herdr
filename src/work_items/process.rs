//! Bounded subprocess execution for work-item background workers.

use std::io;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_DETAIL_CHARS: usize = 200;

/// Runs `command` with piped output, killing it after `timeout`.
pub(crate) fn run_with_timeout(command: Command, timeout: Duration) -> io::Result<Output> {
    run(command, None, timeout)
}

/// Like [`run_with_timeout`], writing `input` to the child's stdin. Secrets passed this
/// way stay out of the process list.
pub(crate) fn run_with_input(
    command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> io::Result<Output> {
    run(command, Some(input), timeout)
}

fn run(mut command: Command, input: Option<Vec<u8>>, timeout: Duration) -> io::Result<Output> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        thread::spawn(move || {
            use io::Write;
            // Dropping stdin afterwards closes it, which ends the child's input.
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = child.stdout.take().map(read_to_end_thread);
    let stderr = child.stderr.take().map(read_to_end_thread);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            // Reader threads are left to finish on their own: a grandchild may
            // still hold the pipes open.
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {} s", timeout.as_secs()),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };
    Ok(Output {
        status,
        stdout: join_reader(stdout),
        stderr: join_reader(stderr),
    })
}

fn read_to_end_thread<R: io::Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    })
}

fn join_reader(handle: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

/// Last non-empty line of process output, trimmed and bounded for display.
pub(crate) fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    line.chars().take(MAX_DETAIL_CHARS).collect()
}

/// Error detail for a finished process: its last stderr line, else stdout, else the exit status.
pub(crate) fn failure_detail(output: &Output) -> String {
    let stderr = last_line(&output.stderr);
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = last_line(&output.stdout);
    if !stdout.is_empty() {
        return stdout;
    }
    match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => "terminated by a signal".into(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn last_line_skips_trailing_blank_lines() {
        assert_eq!(last_line(b"first\nsecond  \n\n"), "second");
    }

    #[test]
    fn slow_command_is_killed_at_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let started = Instant::now();
        let err = run_with_timeout(command, Duration::from_millis(200)).expect_err("times out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
