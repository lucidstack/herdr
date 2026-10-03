//! Bounded subprocess execution for work-item background workers.

use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_DETAIL_CHARS: usize = 200;

/// Runs `command` with piped output, killing it after `timeout`.
pub(crate) fn run_with_timeout(command: Command, timeout: Duration) -> io::Result<Output> {
    run(command, None, timeout, None).map(|(output, _)| output)
}

/// Like [`run_with_timeout`], but keeps at most `max_stdout` bytes of stdout. A command that
/// prints more is killed and the second value is `true`; the output holds what arrived first
/// and its exit status says nothing. Callers whose commands may print far more than they can
/// use (a diff of a huge change) never hold all of it in memory.
pub(crate) fn run_with_timeout_capped(
    command: Command,
    timeout: Duration,
    max_stdout: usize,
) -> io::Result<(Output, bool)> {
    run(command, None, timeout, Some(max_stdout))
}

/// Like [`run_with_timeout`], writing `input` to the child's stdin. Secrets passed this
/// way stay out of the process list.
pub(crate) fn run_with_input(
    command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> io::Result<Output> {
    run(command, Some(input), timeout, None).map(|(output, _)| output)
}

fn run(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    stdout_cap: Option<usize>,
) -> io::Result<(Output, bool)> {
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
    let capped = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().map(|reader| match stdout_cap {
        Some(cap) => read_capped_thread(reader, cap, Arc::clone(&capped)),
        None => read_to_end_thread(reader),
    });
    let stderr = child.stderr.take().map(read_to_end_thread);
    let started = Instant::now();
    // Most commands are done within a few milliseconds, so the first checks come quickly
    // and the gap grows to `POLL_INTERVAL`.
    let mut poll_delay = Duration::from_millis(1);
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
        if capped.load(Ordering::Acquire) {
            break stop(&mut child)?;
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
        thread::sleep(poll_delay);
        poll_delay = (poll_delay * 2).min(POLL_INTERVAL);
    };
    let output = Output {
        status,
        stdout: join_reader(stdout),
        stderr: join_reader(stderr),
    };
    Ok((output, capped.load(Ordering::Acquire)))
}

fn stop(child: &mut Child) -> io::Result<ExitStatus> {
    let _ = child.kill();
    child.wait()
}

fn read_to_end_thread<R: io::Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    })
}

/// Reads until `cap` bytes are held; if the stream goes on, sets `capped`, drops what is
/// beyond and stops reading.
fn read_capped_thread<R: io::Read + Send + 'static>(
    mut reader: R,
    cap: usize,
    capped: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let room = cap - bytes.len();
                    if read > room {
                        bytes.extend_from_slice(&chunk[..room]);
                        capped.store(true, Ordering::Release);
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
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

/// First non-empty line of process output, trimmed and bounded for display.
pub(crate) fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(MAX_DETAIL_CHARS).collect())
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

    fn print_bytes(count: usize) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("head -c {count} /dev/zero")]);
        command
    }

    #[test]
    fn capped_run_keeps_the_first_bytes_and_stops_a_chatty_command() {
        // Far more than the pipe buffer holds: the command would block, not finish, if
        // nothing kept reading or killed it.
        let started = Instant::now();
        let (output, capped) =
            run_with_timeout_capped(print_bytes(50_000_000), Duration::from_secs(20), 1000)
                .expect("runs");
        assert!(capped);
        assert_eq!(output.stdout.len(), 1000);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn capped_run_reports_output_that_fits_as_complete() {
        for (count, cap) in [(999, 1000), (1000, 1000)] {
            let (output, capped) =
                run_with_timeout_capped(print_bytes(count), Duration::from_secs(20), cap)
                    .expect("runs");
            assert!(!capped, "{count} bytes under a cap of {cap}");
            assert_eq!(output.stdout.len(), count);
            assert!(output.status.success());
        }
    }
}
