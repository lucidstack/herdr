use crate::api::schema::{
    EmptyParams, Method, WorkItemChooseParams, WorkItemHideParams, WorkItemLinkParams,
    WorkItemTarget,
};

// Output is the JSON API response, like the other socket commands.
pub(super) fn run_work_item_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_work_item_help();
        return Ok(2);
    };
    let rest = &args[1..];
    match subcommand {
        "list" if rest.is_empty() => send(
            "cli:work-item:list",
            Method::WorkItemList(EmptyParams::default()),
        ),
        "mark-seen" => match rest {
            [item_id] => send(
                "cli:work-item:mark-seen",
                Method::WorkItemMarkSeen(WorkItemTarget {
                    item_id: item_id.clone(),
                }),
            ),
            _ => usage("herdr work-item mark-seen ITEM_ID"),
        },
        "choose" => match rest {
            [item_id, choice_id] => send(
                "cli:work-item:choose",
                Method::WorkItemChoose(WorkItemChooseParams {
                    item_id: item_id.clone(),
                    choice_id: choice_id.clone(),
                }),
            ),
            _ => usage("herdr work-item choose ITEM_ID CHOICE_ID"),
        },
        "dismiss" => match rest {
            [item_id] => send(
                "cli:work-item:dismiss",
                Method::WorkItemHide(WorkItemHideParams {
                    item_id: item_id.clone(),
                    snooze_seconds: None,
                }),
            ),
            _ => usage("herdr work-item dismiss ITEM_ID"),
        },
        "snooze" => {
            let (item_id, duration) = match rest {
                [item_id] => (item_id, "1h"),
                [item_id, flag, duration] if flag == "--for" => (item_id, duration.as_str()),
                _ => return usage("herdr work-item snooze ITEM_ID [--for DURATION]"),
            };
            let Some(seconds) = parse_duration(duration) else {
                eprintln!("invalid duration {duration:?}; use for example 30m, 2h or 1d");
                return Ok(2);
            };
            send(
                "cli:work-item:snooze",
                Method::WorkItemHide(WorkItemHideParams {
                    item_id: item_id.clone(),
                    snooze_seconds: Some(seconds),
                }),
            )
        }
        "link" => match rest {
            [item_id, workspace_id] => send(
                "cli:work-item:link",
                Method::WorkItemLink(WorkItemLinkParams {
                    item_id: item_id.clone(),
                    workspace_id: super::normalize_workspace_id(workspace_id),
                }),
            ),
            _ => usage("herdr work-item link ITEM_ID WORKSPACE_ID"),
        },
        "unhide" => match rest {
            [item_id] => send(
                "cli:work-item:unhide",
                Method::WorkItemUnhide(WorkItemTarget {
                    item_id: item_id.clone(),
                }),
            ),
            _ => usage("herdr work-item unhide ITEM_ID"),
        },
        "help" | "--help" | "-h" => {
            print_work_item_help();
            Ok(0)
        }
        _ => {
            print_work_item_help();
            Ok(2)
        }
    }
}

fn send(id: &'static str, method: Method) -> std::io::Result<i32> {
    super::runtime::print_method_response(id, method)
}

fn usage(line: &str) -> std::io::Result<i32> {
    eprintln!("usage: {line}");
    Ok(2)
}

/// `90`, `90s`, `30m`, `2h` or `1d`, in seconds.
fn parse_duration(value: &str) -> Option<u64> {
    let (number, unit) = match value.char_indices().last()? {
        (index, unit) if unit.is_ascii_alphabetic() => (&value[..index], Some(unit)),
        _ => (value, None),
    };
    let number: u64 = number.parse().ok()?;
    let scale = match unit {
        None | Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => 24 * 60 * 60,
        Some(_) => return None,
    };
    number.checked_mul(scale).filter(|seconds| *seconds > 0)
}

fn print_work_item_help() {
    eprintln!("herdr work-item commands:");
    eprintln!("  herdr work-item list");
    eprintln!("  herdr work-item choose ITEM_ID CHOICE_ID");
    eprintln!("  herdr work-item mark-seen ITEM_ID");
    eprintln!("  herdr work-item dismiss ITEM_ID");
    eprintln!("  herdr work-item snooze ITEM_ID [--for DURATION]   (default 1h; e.g. 30m, 2h, 1d)");
    eprintln!("  herdr work-item unhide ITEM_ID");
    eprintln!("  herdr work-item link ITEM_ID WORKSPACE_ID");
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn durations_accept_seconds_minutes_hours_and_days() {
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("3w"), None);
        assert_eq!(parse_duration("h"), None);
    }
}
