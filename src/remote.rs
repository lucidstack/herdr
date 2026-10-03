mod allow_list;
mod args;
mod attach;
mod host;
mod process;
mod restart_policy;
mod saved;
#[cfg(unix)]
mod ssh_agent;

#[cfg(unix)]
pub(crate) use allow_list::forward_filtered;
pub(crate) use allow_list::AllowList;
pub(crate) use args::*;
pub(crate) use attach::*;
pub(crate) use host::run_remote_client_bridge;
pub(crate) use saved::*;

const BRIDGE_USAGE: &str =
    "usage: herdr remote-api-bridge [--check | --allow <method[,method...]>]";

#[derive(Debug)]
enum BridgeMode {
    Check,
    Raw,
    Filtered(AllowList),
}

fn parse_bridge_args(args: &[String]) -> Result<BridgeMode, String> {
    match args {
        [] => Ok(BridgeMode::Raw),
        [flag] if flag == "--check" => Ok(BridgeMode::Check),
        [flag, spec] if flag == "--allow" => AllowList::parse(spec).map(BridgeMode::Filtered),
        [flag] if flag.starts_with("--allow=") => {
            AllowList::parse(&flag["--allow=".len()..]).map(BridgeMode::Filtered)
        }
        _ => Err("unexpected arguments".to_owned()),
    }
}

pub(crate) fn run_remote_api_bridge(args: &[String]) -> std::io::Result<()> {
    let mode = parse_bridge_args(args).map_err(|message| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{message}; {BRIDGE_USAGE}"),
        )
    })?;
    if matches!(mode, BridgeMode::Check) {
        println!("herdr-api-bridge-v1");
        return Ok(());
    }

    let path = crate::api::socket_path();
    let stream = crate::ipc::connect_local_stream(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to connect to remote Herdr API socket {}: {error}",
                path.display()
            ),
        )
    })?;
    match mode {
        BridgeMode::Filtered(allow) => {
            crate::platform::forward_remote_bridge_stdio_filtered(stream, allow)
        }
        BridgeMode::Raw | BridgeMode::Check => {
            crate::platform::forward_remote_bridge_stdio(stream, false)
        }
    }
}

pub(crate) fn print_saved_ssh_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_host_key_error(err) {
        eprintln!(
            "hint: saved machines use strict host-key checking; add the host key to the configured known_hosts file, then retry."
        );
    } else {
        print_remote_error_hint(err, target);
    }
}

pub(crate) fn print_remote_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_auth_error(err) {
        eprintln!(
            "hint: verify SSH access first with `{}`.",
            ssh_check_command(target)
        );
        eprintln!(
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before running `herdr --remote`."
        );
    }
}

fn is_remote_host_key_error(err: &std::io::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("host key verification failed")
        || message.contains("remote host identification has changed")
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    let message = err.to_string();
    message.contains("Permission denied")
        && (message.contains("(publickey")
            || message.contains("(keyboard-interactive")
            || message.contains("(password"))
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_key_error_matches_ssh_diagnostics() {
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
        ] {
            assert!(is_remote_host_key_error(&std::io::Error::other(message)));
        }
        assert!(!is_remote_host_key_error(&std::io::Error::other(
            "server closed connection"
        )));
    }

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let err = std::io::Error::other(
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let err = std::io::Error::other(
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }

    fn bridge_args(args: &[&str]) -> Result<BridgeMode, String> {
        parse_bridge_args(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn bridge_without_arguments_stays_a_raw_pipe() {
        assert!(matches!(bridge_args(&[]), Ok(BridgeMode::Raw)));
        assert!(matches!(bridge_args(&["--check"]), Ok(BridgeMode::Check)));
    }

    #[test]
    fn bridge_allow_accepts_a_valid_list_in_both_spellings() {
        let spec = "work_item.list,agent.list,plugin.action.invoke:lucidstack.herdr-push/register";
        assert!(matches!(
            bridge_args(&["--allow", spec]),
            Ok(BridgeMode::Filtered(_))
        ));
        assert!(matches!(
            bridge_args(&[&format!("--allow={spec}")]),
            Ok(BridgeMode::Filtered(_))
        ));
    }

    #[test]
    fn bridge_allow_rejects_typos_and_malformed_entries() {
        for args in [
            &["--allow", "agent.lsit"][..],
            &["--allow=agent.list,nope"],
            &["--allow", "plugin.action.invoke:no-slash"],
            &["--allow", ""],
        ] {
            assert!(bridge_args(args).is_err(), "{args:?} was accepted");
        }
    }

    #[test]
    fn bridge_rejects_stray_and_missing_arguments() {
        for args in [
            &["--allow"][..],
            &["--allow", "agent.list", "extra"],
            &["--check", "--allow", "agent.list"],
            &["--bogus"],
        ] {
            assert!(bridge_args(args).is_err(), "{args:?} was accepted");
        }
    }
}
