// Only the Unix bridge forwards through the filter; other platforms parse the list and refuse
// `--allow`, which leaves the forwarding loop unused there.
#![cfg_attr(not(unix), allow(dead_code))]

//! Method allow-list for `herdr remote-api-bridge --allow`.
//!
//! A key whose `authorized_keys` entry forces the bridge can be limited to a few API methods.
//! The bridge then reads the client's newline-delimited requests itself, forwards allowed lines to
//! the API socket byte for byte, and answers every other line on stdout without forwarding it.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::api::schema::{ErrorBody, Method};

const PLUGIN_ACTION_INVOKE: &str = "plugin.action.invoke";
const MAX_REQUEST_LINE_BYTES: usize = crate::api::MAX_INITIAL_REQUEST_BYTES;

/// API methods a restricted bridge forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AllowList {
    methods: BTreeSet<String>,
    /// `(plugin_id, action_id)` pairs `plugin.action.invoke` may target when the method isn't
    /// allowed outright.
    plugin_actions: BTreeSet<(String, String)>,
}

impl AllowList {
    /// Parses a comma-separated list of API method names. `plugin.action.invoke` may also be
    /// given as `plugin.action.invoke:<plugin_id>/<action_id>` to allow only that action.
    pub(crate) fn parse(spec: &str) -> Result<Self, String> {
        let known = known_method_names();
        let mut methods = BTreeSet::new();
        let mut plugin_actions = BTreeSet::new();
        for entry in spec.split(',').map(str::trim) {
            if entry.is_empty() {
                return Err(format!("empty entry in allow list {spec:?}"));
            }
            if let Some((method, target)) = entry.split_once(':') {
                if method != PLUGIN_ACTION_INVOKE {
                    return Err(format!(
                        "{entry:?}: only {PLUGIN_ACTION_INVOKE} can be limited to specific targets"
                    ));
                }
                let Some((plugin_id, action_id)) =
                    target.split_once('/').filter(|(plugin_id, action_id)| {
                        !plugin_id.is_empty() && !action_id.is_empty()
                    })
                else {
                    return Err(format!(
                        "{entry:?}: expected {PLUGIN_ACTION_INVOKE}:<plugin_id>/<action_id>"
                    ));
                };
                plugin_actions.insert((plugin_id.to_owned(), action_id.to_owned()));
            } else if known.contains(&entry) {
                methods.insert(entry.to_owned());
            } else {
                return Err(format!("unknown API method {entry:?}"));
            }
        }
        Ok(Self {
            methods,
            plugin_actions,
        })
    }

    fn permits(&self, method: &str, line: &[u8]) -> bool {
        if self.methods.contains(method) {
            return true;
        }
        if method != PLUGIN_ACTION_INVOKE || self.plugin_actions.is_empty() {
            return false;
        }

        #[derive(Deserialize)]
        struct Invoke {
            params: InvokeParams,
        }
        #[derive(Deserialize)]
        struct InvokeParams {
            plugin_id: Option<String>,
            action_id: String,
        }

        // An action without an explicit plugin could resolve to any plugin's action.
        let Ok(Invoke {
            params:
                InvokeParams {
                    plugin_id: Some(plugin_id),
                    action_id,
                },
        }) = serde_json::from_slice(line)
        else {
            return false;
        };
        self.plugin_actions.contains(&(plugin_id, action_id))
    }
}

/// Every method name the API accepts, taken from `Method`'s serde variant list so it can't drift.
fn known_method_names() -> &'static [&'static str] {
    #[derive(Debug)]
    enum Probe {
        Variants(&'static [&'static str]),
        Other,
    }

    impl std::fmt::Display for Probe {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("method name probe")
        }
    }

    impl std::error::Error for Probe {}

    impl serde::de::Error for Probe {
        fn custom<T: std::fmt::Display>(_message: T) -> Self {
            Self::Other
        }

        fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
            Self::Variants(expected)
        }
    }

    // An empty method name is never a variant, so serde reports the full list it expected.
    let probe = serde::de::value::MapDeserializer::<_, Probe>::new(std::iter::once(("method", "")));
    match Method::deserialize(probe) {
        Err(Probe::Variants(names)) => names,
        _ => &[],
    }
}

/// What the bridge does with one client request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineVerdict {
    /// Forward the line to the API socket unchanged.
    Forward,
    /// Don't forward; write this newline-terminated response to the client instead.
    Reply(Vec<u8>),
}

/// Decides one client line (with or without its trailing newline).
pub(crate) fn filter_line(allow: &AllowList, line: &[u8]) -> LineVerdict {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        id: Option<serde_json::Value>,
        method: String,
    }

    // Duplicate `id` or `method` keys fail here, so the server can't see a different method.
    let envelope = match serde_json::from_slice::<Envelope>(line) {
        Ok(envelope) => envelope,
        Err(error) => {
            return error_reply(
                &empty_id(),
                "invalid_request",
                format!("invalid request: {error}"),
            )
        }
    };
    if allow.permits(&envelope.method, line) {
        return LineVerdict::Forward;
    }
    error_reply(
        &envelope.id.unwrap_or_else(empty_id),
        "method_not_allowed",
        format!("{} isn't allowed for this key", envelope.method),
    )
}

fn empty_id() -> serde_json::Value {
    serde_json::Value::String(String::new())
}

fn error_reply(id: &serde_json::Value, code: &str, message: String) -> LineVerdict {
    #[derive(serde::Serialize)]
    struct BridgeError<'a> {
        id: &'a serde_json::Value,
        error: ErrorBody,
    }

    let response = BridgeError {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    };
    let mut bytes = serde_json::to_vec(&response).unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to encode remote bridge error response");
        br#"{"id":"","error":{"code":"internal_error","message":"failed to encode bridge error"}}"#
            .to_vec()
    });
    bytes.push(b'\n');
    LineVerdict::Reply(bytes)
}

/// Forwards stdio through the allow-list. Client lines are filtered on a detached thread that
/// calls `finish_upload` at client EOF; server output is copied to `stdout` whole lines at a time
/// until the server closes, sharing one lock with the filter's replies so lines never interleave.
pub(crate) fn forward_filtered<C, U, D, O>(
    allow: AllowList,
    client: C,
    upload: U,
    finish_upload: impl FnOnce(U) + Send + 'static,
    server: D,
    stdout: O,
) -> io::Result<()>
where
    C: Read + Send + 'static,
    U: Write + Send + 'static,
    D: Read,
    O: Write + Send + 'static,
{
    let stdout = Arc::new(Mutex::new(stdout));
    let replies = Arc::clone(&stdout);
    let _upload = std::thread::spawn(move || {
        let mut upload = upload;
        if let Err(error) =
            filter_client_lines(&allow, &mut BufReader::new(client), &mut upload, &replies)
        {
            tracing::debug!(%error, "remote bridge stopped reading client requests");
        }
        finish_upload(upload);
    });
    copy_server_lines(&mut BufReader::new(server), &stdout)
}

fn filter_client_lines<C: BufRead, U: Write, O: Write>(
    allow: &AllowList,
    client: &mut C,
    upload: &mut U,
    stdout: &Mutex<O>,
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        let verdict = match read_client_line(client, &mut line, MAX_REQUEST_LINE_BYTES)? {
            ClientLine::Eof => return Ok(()),
            ClientLine::Complete => filter_line(allow, &line),
            ClientLine::TooLong => error_reply(
                &empty_id(),
                "invalid_request",
                "api request line is too large".into(),
            ),
        };
        match verdict {
            LineVerdict::Forward => {
                upload.write_all(&line)?;
                upload.flush()?;
            }
            LineVerdict::Reply(response) => write_whole(stdout, &response)?,
        }
    }
}

fn copy_server_lines<D: BufRead, O: Write>(server: &mut D, stdout: &Mutex<O>) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        // A trailing partial line is only returned at EOF, when nothing else can follow it.
        if server.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        write_whole(stdout, &line)?;
    }
}

fn write_whole<O: Write>(stdout: &Mutex<O>, bytes: &[u8]) -> io::Result<()> {
    let mut stdout = stdout
        .lock()
        .map_err(|_| io::Error::other("remote bridge stdout lock poisoned"))?;
    stdout.write_all(bytes)?;
    stdout.flush()
}

#[derive(Debug, PartialEq, Eq)]
enum ClientLine {
    Eof,
    /// `line` holds the request, including its newline unless it ended at EOF.
    Complete,
    /// The request exceeded the cap and was discarded through its newline.
    TooLong,
}

fn read_client_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<ClientLine> {
    line.clear();
    let mut too_long = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(if too_long {
                ClientLine::TooLong
            } else if line.is_empty() {
                ClientLine::Eof
            } else {
                ClientLine::Complete
            });
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let chunk = &available[..newline.map_or(available.len(), |index| index + 1)];
        let consumed = chunk.len();
        if !too_long {
            let content_bytes = line.len() + chunk.len() - usize::from(newline.is_some());
            if content_bytes > max_bytes {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(if too_long {
                ClientLine::TooLong
            } else {
                ClientLine::Complete
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(spec: &str) -> AllowList {
        AllowList::parse(spec).unwrap()
    }

    fn reply_json(verdict: LineVerdict) -> serde_json::Value {
        let LineVerdict::Reply(bytes) = verdict else {
            panic!("expected a reply, got {verdict:?}");
        };
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn known_method_names_come_from_the_request_schema() {
        let names = known_method_names();
        assert!(names.contains(&"ping"));
        assert!(names.contains(&"agent.list"));
        assert!(names.contains(&PLUGIN_ACTION_INVOKE));
        assert!(!names.contains(&""));
    }

    #[test]
    fn parse_accepts_methods_and_qualified_plugin_actions() {
        let list = allow(
            "work_item.list, agent.list,agent.prompt,plugin.action.invoke:lucidstack.herdr-push/register",
        );
        assert_eq!(
            list.methods.iter().map(String::as_str).collect::<Vec<_>>(),
            ["agent.list", "agent.prompt", "work_item.list"]
        );
        assert_eq!(
            list.plugin_actions.iter().cloned().collect::<Vec<_>>(),
            [("lucidstack.herdr-push".to_owned(), "register".to_owned())]
        );
    }

    #[test]
    fn parse_rejects_unknown_method() {
        let error = AllowList::parse("agent.list,agent.lsit").unwrap_err();
        assert!(error.contains("agent.lsit"), "{error}");
    }

    #[test]
    fn parse_rejects_malformed_entries() {
        for spec in [
            "",
            "agent.list,",
            "plugin.action.invoke:",
            "plugin.action.invoke:only-plugin",
            "plugin.action.invoke:/register",
            "plugin.action.invoke:plugin/",
            "agent.prompt:w1/p1",
        ] {
            assert!(AllowList::parse(spec).is_err(), "{spec:?} was accepted");
        }
    }

    #[test]
    fn allowed_method_is_forwarded_verbatim() {
        let line = br#"{"id":"a","method":"agent.list","params":{}}"#;
        assert_eq!(
            filter_line(&allow("agent.list"), line),
            LineVerdict::Forward
        );
    }

    #[test]
    fn denied_method_is_answered_with_its_string_id() {
        let response = reply_json(filter_line(
            &allow("agent.list"),
            br#"{"id":"req-7","method":"server.stop","params":{}}"#,
        ));
        assert_eq!(
            response,
            serde_json::json!({
                "id": "req-7",
                "error": {
                    "code": "method_not_allowed",
                    "message": "server.stop isn't allowed for this key",
                },
            })
        );
    }

    #[test]
    fn denied_method_echoes_a_numeric_id() {
        let response = reply_json(filter_line(
            &allow("agent.list"),
            br#"{"id":42,"method":"pane.send_text","params":{}}"#,
        ));
        assert_eq!(response["id"], 42);
        assert_eq!(response["error"]["code"], "method_not_allowed");
    }

    #[test]
    fn denial_message_starts_with_the_id_field() {
        let LineVerdict::Reply(bytes) = filter_line(
            &allow("agent.list"),
            br#"{"id":"x","method":"server.stop"}"#,
        ) else {
            panic!("expected a reply");
        };
        assert!(bytes.starts_with(br#"{"id":"x","error":"#));
    }

    #[test]
    fn qualified_plugin_actions_allow_only_listed_actions() {
        let list = allow("plugin.action.invoke:lucidstack.herdr-push/register");
        let invoke = |params: &str| {
            filter_line(
                &list,
                format!(r#"{{"id":"p","method":"plugin.action.invoke","params":{params}}}"#)
                    .as_bytes(),
            )
        };

        assert_eq!(
            invoke(r#"{"plugin_id":"lucidstack.herdr-push","action_id":"register","input":"x"}"#),
            LineVerdict::Forward
        );
        for params in [
            r#"{"plugin_id":"lucidstack.herdr-push","action_id":"unregister"}"#,
            r#"{"plugin_id":"other","action_id":"register"}"#,
            r#"{"action_id":"register"}"#,
            r#"{"plugin_id":"lucidstack.herdr-push","plugin_id":"lucidstack.herdr-push","action_id":"register"}"#,
            "null",
        ] {
            let response = reply_json(invoke(params));
            assert_eq!(response["id"], "p", "{params}");
            assert_eq!(response["error"]["code"], "method_not_allowed", "{params}");
        }
    }

    #[test]
    fn bare_plugin_action_invoke_allows_any_action() {
        let line = br#"{"id":"p","method":"plugin.action.invoke","params":{"plugin_id":"any","action_id":"thing"}}"#;
        assert_eq!(
            filter_line(&allow("plugin.action.invoke"), line),
            LineVerdict::Forward
        );
    }

    #[test]
    fn unparseable_lines_are_invalid_requests_without_an_id() {
        let list = allow("agent.list");
        for line in [
            &b"not json\n"[..],
            b"\n",
            br#"{"id":"a"}"#,
            br#"{"id":"a","method":"server.stop","method":"agent.list"}"#,
        ] {
            let response = reply_json(filter_line(&list, line));
            assert_eq!(response["id"], "", "{line:?}");
            assert_eq!(response["error"]["code"], "invalid_request", "{line:?}");
        }
    }

    #[test]
    fn over_long_line_is_refused_and_the_next_line_still_read() {
        let mut input = vec![b'x'; 10];
        input.extend_from_slice(b"\nok\n");
        let mut reader = BufReader::with_capacity(3, input.as_slice());
        let mut line = Vec::new();

        assert_eq!(
            read_client_line(&mut reader, &mut line, 9).unwrap(),
            ClientLine::TooLong
        );
        assert_eq!(
            read_client_line(&mut reader, &mut line, 9).unwrap(),
            ClientLine::Complete
        );
        assert_eq!(line, b"ok\n");
        assert_eq!(
            read_client_line(&mut reader, &mut line, 9).unwrap(),
            ClientLine::Eof
        );
    }

    #[test]
    fn line_at_the_cap_is_kept() {
        let mut input = vec![b'x'; 9];
        input.push(b'\n');
        let mut reader = BufReader::with_capacity(4, input.as_slice());
        let mut line = Vec::new();

        assert_eq!(
            read_client_line(&mut reader, &mut line, 9).unwrap(),
            ClientLine::Complete
        );
        assert_eq!(line, input);
    }

    #[test]
    fn over_long_request_is_answered_and_not_forwarded() {
        let mut input = br#"{"id":"big","method":"agent.list","params":{"pad":""#.to_vec();
        input.resize(MAX_REQUEST_LINE_BYTES + 1, b'x');
        input.extend_from_slice(b"\"}}\n");
        let stdout = Mutex::new(Vec::new());
        let mut upload = Vec::new();

        filter_client_lines(
            &allow("agent.list"),
            &mut input.as_slice(),
            &mut upload,
            &stdout,
        )
        .unwrap();

        assert!(upload.is_empty());
        let response: serde_json::Value =
            serde_json::from_slice(&stdout.into_inner().unwrap()).unwrap();
        assert_eq!(response["id"], "");
        assert_eq!(response["error"]["code"], "invalid_request");
    }

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            // Short writes make any interleaving of two writers visible.
            let len = bytes.len().min(7);
            self.0.lock().unwrap().extend_from_slice(&bytes[..len]);
            std::thread::yield_now();
            Ok(len)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn forwarding_keeps_output_lines_whole_and_ordered() {
        let mut client = Vec::new();
        let mut allowed = Vec::new();
        let mut denied = Vec::new();
        for index in 0..40 {
            let line = if index % 3 == 0 {
                format!("{{\"id\":\"deny-{index}\",\"method\":\"server.stop\"}}\n")
            } else {
                format!("{{\"id\":\"allow-{index}\",\"method\":\"agent.list\",\"params\":{{}}}}\n")
            };
            if index % 3 == 0 {
                denied.push(format!("deny-{index}"));
            } else {
                allowed.push(line.clone());
            }
            client.extend_from_slice(line.as_bytes());
        }

        let (mut server_requests, upload) = io::pipe().unwrap();
        let (server_responses, mut server_writer) = io::pipe().unwrap();
        let server = std::thread::spawn(move || {
            let mut received = Vec::new();
            let mut requests = BufReader::new(&mut server_requests);
            let mut line = String::new();
            while requests.read_line(&mut line).unwrap() > 0 {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let response = serde_json::json!({
                    "id": request["id"],
                    "result": { "padding": "r".repeat(300) },
                });
                writeln!(server_writer, "{response}").unwrap();
                received.push(std::mem::take(&mut line));
            }
            received
        });
        let stdout = SharedOutput::default();

        forward_filtered(
            allow("agent.list"),
            io::Cursor::new(client),
            upload,
            drop,
            server_responses,
            stdout.clone(),
        )
        .unwrap();

        assert_eq!(server.join().unwrap(), allowed);
        let output = String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap();
        assert!(output.ends_with('\n'));
        let mut answered = Vec::new();
        let mut rejected = Vec::new();
        for line in output.lines() {
            let response: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("torn output line {line:?}: {error}"));
            let id = response["id"].as_str().unwrap().to_owned();
            if response.get("result").is_some() {
                answered.push(id);
            } else {
                assert_eq!(response["error"]["code"], "method_not_allowed");
                rejected.push(id);
            }
        }
        assert_eq!(rejected, denied);
        assert_eq!(
            answered,
            allowed
                .iter()
                .map(
                    |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                )
                .collect::<Vec<_>>()
        );
    }
}
