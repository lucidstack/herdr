use crate::api::schema::{
    Method, Request, ServiceAddParams, ServiceListParams, ServiceRemoveParams,
};

const ADD_USAGE: &str =
    "usage: herdr service add <label> <url> [--workspace <workspace_id>] [--source TEXT]";
const LIST_USAGE: &str = "usage: herdr service list [--workspace <workspace_id>]";
const REMOVE_USAGE: &str =
    "usage: herdr service remove (<service_id> | --label <label>) [--workspace <workspace_id>]";

pub(super) fn run_service_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_service_help();
        return Ok(2);
    };

    match subcommand {
        "add" => service_add(&args[1..]),
        "list" => service_list(&args[1..]),
        "remove" => service_remove(&args[1..]),
        "help" | "--help" | "-h" => {
            print_service_help();
            Ok(0)
        }
        _ => {
            print_service_help();
            Ok(2)
        }
    }
}

/// Where a service request is attributed when `--workspace` is absent: the
/// calling pane's workspace via `HERDR_WORKSPACE_ID`, else the calling pane
/// itself via `HERDR_PANE_ID` (the server resolves its workspace). Both are
/// ignored when the command is forwarded to a remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attribution {
    workspace_id: Option<String>,
    pane_id: Option<String>,
}

fn resolve_attribution(explicit_workspace_id: Option<String>) -> Option<Attribution> {
    if let Some(workspace_id) = explicit_workspace_id {
        return Some(Attribution {
            workspace_id: Some(super::normalize_workspace_id(&workspace_id)),
            pane_id: None,
        });
    }
    if super::target::is_remote() {
        return None;
    }
    if let Some(workspace_id) = std::env::var(crate::integration::HERDR_WORKSPACE_ID_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Some(Attribution {
            workspace_id: Some(workspace_id),
            pane_id: None,
        });
    }
    super::target::caller_pane_id().map(|pane_id| Attribution {
        workspace_id: None,
        pane_id: Some(pane_id),
    })
}

fn attribution_or_usage_error(
    explicit_workspace_id: Option<String>,
    usage: &str,
) -> Result<Attribution, i32> {
    resolve_attribution(explicit_workspace_id).ok_or_else(|| {
        eprintln!("error: --workspace is required when not running inside a Herdr pane; {usage}");
        2
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AddArgs {
    label: String,
    url: String,
    workspace_id: Option<String>,
    source: Option<String>,
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let expanded = super::expand_equals_args(args, &["--workspace", "--source"]);
    let mut positional = Vec::new();
    let mut workspace_id = None;
    let mut source = None;
    let mut index = 0;
    while index < expanded.len() {
        match expanded[index].as_str() {
            "--workspace" => {
                workspace_id = Some(flag_value(&expanded, index, "--workspace")?);
                index += 2;
            }
            "--source" => {
                source = Some(flag_value(&expanded, index, "--source")?);
                index += 2;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag: {flag}")),
            value => {
                positional.push(value.to_string());
                index += 1;
            }
        }
    }
    let [label, url] = <[String; 2]>::try_from(positional)
        .map_err(|_| "service add takes exactly <label> and <url>".to_string())?;
    Ok(AddArgs {
        label,
        url,
        workspace_id,
        source,
    })
}

fn service_add(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_add_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("{ADD_USAGE}");
            return Ok(2);
        }
    };
    let attribution = match attribution_or_usage_error(parsed.workspace_id, ADD_USAGE) {
        Ok(attribution) => attribution,
        Err(code) => return Ok(code),
    };
    print_method_response(
        "cli:service:add",
        Method::ServiceAdd(ServiceAddParams {
            workspace_id: attribution.workspace_id,
            pane_id: attribution.pane_id,
            label: parsed.label,
            url: parsed.url,
            source: parsed.source,
        }),
    )
}

fn service_list(args: &[String]) -> std::io::Result<i32> {
    let expanded = super::expand_equals_args(args, &["--workspace"]);
    let workspace_id = match expanded.as_slice() {
        [] => None,
        [flag, value] if flag == "--workspace" => Some(super::normalize_workspace_id(value)),
        _ => {
            eprintln!("{LIST_USAGE}");
            return Ok(2);
        }
    };
    print_method_response(
        "cli:service:list",
        Method::ServiceList(ServiceListParams { workspace_id }),
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RemoveArgs {
    id: Option<u64>,
    label: Option<String>,
    workspace_id: Option<String>,
}

fn parse_remove_args(args: &[String]) -> Result<RemoveArgs, String> {
    let expanded = super::expand_equals_args(args, &["--workspace", "--label"]);
    let mut parsed = RemoveArgs::default();
    let mut index = 0;
    while index < expanded.len() {
        match expanded[index].as_str() {
            "--workspace" => {
                parsed.workspace_id = Some(flag_value(&expanded, index, "--workspace")?);
                index += 2;
            }
            "--label" => {
                parsed.label = Some(flag_value(&expanded, index, "--label")?);
                index += 2;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag: {flag}")),
            value => {
                if parsed.id.is_some() {
                    return Err(format!("unexpected argument: {value}"));
                }
                parsed.id = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("service id must be a number: {value}"))?,
                );
                index += 1;
            }
        }
    }
    match (parsed.id, &parsed.label) {
        (Some(_), Some(_)) => Err("pass either <service_id> or --label, not both".to_string()),
        (None, None) => Err("a <service_id> or --label is required".to_string()),
        _ => Ok(parsed),
    }
}

fn service_remove(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_remove_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("{REMOVE_USAGE}");
            return Ok(2);
        }
    };
    let attribution = match attribution_or_usage_error(parsed.workspace_id, REMOVE_USAGE) {
        Ok(attribution) => attribution,
        Err(code) => return Ok(code),
    };
    print_method_response(
        "cli:service:remove",
        Method::ServiceRemove(ServiceRemoveParams {
            workspace_id: attribution.workspace_id,
            pane_id: attribution.pane_id,
            id: parsed.id,
            label: parsed.label,
        }),
    )
}

fn flag_value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn print_method_response(id: &'static str, method: Method) -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

fn print_service_help() {
    eprintln!("herdr service commands:");
    eprintln!("  herdr service add <label> <url> [--workspace <workspace_id>] [--source TEXT]");
    eprintln!("  herdr service list [--workspace <workspace_id>]");
    eprintln!(
        "  herdr service remove (<service_id> | --label <label>) [--workspace <workspace_id>]"
    );
    eprintln!();
    eprintln!("Inside a Herdr pane the workspace is taken from HERDR_WORKSPACE_ID (or the pane's");
    eprintln!("workspace via HERDR_PANE_ID). URLs may be http(s)://..., host:port, or :port.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn add_accepts_label_url_and_flags_in_any_order() {
        let parsed = parse_add_args(&args(&[
            "--source=agent",
            "Rails",
            "--workspace",
            "w1",
            ":3000",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            AddArgs {
                label: "Rails".into(),
                url: ":3000".into(),
                workspace_id: Some("w1".into()),
                source: Some("agent".into()),
            }
        );
    }

    #[test]
    fn add_rejects_missing_or_extra_positionals() {
        assert!(parse_add_args(&args(&["Rails"])).is_err());
        assert!(parse_add_args(&args(&["Rails", ":3000", "extra"])).is_err());
        assert!(parse_add_args(&args(&["Rails", ":3000", "--workspace"])).is_err());
    }

    #[test]
    fn remove_requires_exactly_one_selector() {
        assert!(parse_remove_args(&args(&[])).is_err());
        assert!(parse_remove_args(&args(&["3", "--label", "Rails"])).is_err());
        assert!(parse_remove_args(&args(&["three"])).is_err());
        assert_eq!(
            parse_remove_args(&args(&["--label", "Rails"])).unwrap(),
            RemoveArgs {
                id: None,
                label: Some("Rails".into()),
                workspace_id: None,
            }
        );
        assert_eq!(parse_remove_args(&args(&["3"])).unwrap().id, Some(3));
    }

    #[test]
    fn explicit_workspace_wins_over_environment() {
        let attribution = resolve_attribution(Some("w9".into())).unwrap();
        assert_eq!(attribution.workspace_id.as_deref(), Some("w9"));
        assert_eq!(attribution.pane_id, None);
    }
}
