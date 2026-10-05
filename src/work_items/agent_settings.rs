//! The model and effort level a work-item choice can start its agent with, and the
//! arguments that carry them to the agents Herdr knows how to pass them to.

use crate::api::schema::WorkItemChoiceAgentInfo;

/// Models offered for a choice's agent, as both Claude Code and omp take them.
const MODELS: &[&str] = &["haiku", "sonnet", "opus", "fable"];
/// Effort levels offered, least first: Claude Code's `--effort`, and levels omp's
/// `--thinking` takes too.
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

const MODEL_FLAG: &str = "--model";

/// The flag that sets the effort of `agent`, `None` for an agent Herdr passes neither to.
fn effort_flag(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("--effort"),
        "omp" => Some("--thinking"),
        _ => None,
    }
}

/// What a choice starting `agent` with `args` offers, `None` when Herdr cannot pass a model
/// or an effort to that agent.
pub(crate) fn choice_agent_info(agent: &str, args: &[String]) -> Option<WorkItemChoiceAgentInfo> {
    let effort_flag = effort_flag(agent)?;
    Some(WorkItemChoiceAgentInfo {
        models: MODELS.iter().map(|model| (*model).into()).collect(),
        efforts: EFFORTS.iter().map(|effort| (*effort).into()).collect(),
        configured_model: flag_value(args, MODEL_FLAG),
        configured_effort: flag_value(args, effort_flag),
    })
}

/// Why a model or effort cannot start a choice's agent: an `(code, message)` API error.
pub(crate) fn check(
    info: Option<&WorkItemChoiceAgentInfo>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), (&'static str, String)> {
    let offered = |values: Option<&Vec<String>>, value: &str| {
        values.is_some_and(|values| values.iter().any(|offered| offered == value))
    };
    if let Some(model) = model {
        if !offered(info.map(|info| &info.models), model) {
            return Err(("unknown_model", format!("unknown model {model}")));
        }
    }
    if let Some(effort) = effort {
        if !offered(info.map(|info| &info.efforts), effort) {
            return Err(("unknown_effort", format!("unknown effort {effort}")));
        }
    }
    Ok(())
}

/// `args` with `model` and `effort` in place of any the configuration gives `agent`. Each
/// left `None` keeps what is configured.
pub(crate) fn with_settings(
    agent: &str,
    args: &[String],
    model: Option<&str>,
    effort: Option<&str>,
) -> Vec<String> {
    let mut args = args.to_vec();
    let settings = [(Some(MODEL_FLAG), model), (effort_flag(agent), effort)];
    for (flag, value) in settings {
        if let (Some(flag), Some(value)) = (flag, value) {
            args = without_flag(&args, flag);
            args.push(flag.into());
            args.push(value.into());
        }
    }
    args
}

/// The value `args` give `flag`, as `--flag value` or `--flag=value`; the last one wins, as
/// it does for the agents.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut value = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            value = iter.next().cloned();
        } else if let Some(inline) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            value = Some(inline.to_string());
        }
    }
    value
}

/// `args` without `flag` and its value, in either form.
fn without_flag(args: &[String], flag: &str) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            iter.next();
        } else if !arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
        {
            kept.push(arg.clone());
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn a_choice_offers_settings_only_for_agents_herdr_passes_them_to() {
        assert!(choice_agent_info("claude", &[]).is_some());
        assert!(choice_agent_info("omp", &[]).is_some());
        assert!(choice_agent_info("codex", &[]).is_none());
        assert!(choice_agent_info("", &[]).is_none());
    }

    #[test]
    fn the_configured_settings_are_read_from_either_form_of_the_flags() {
        let info = choice_agent_info("claude", &args(&["--model=sonnet", "--effort", "high"]))
            .expect("claude takes settings");
        assert_eq!(info.configured_model.as_deref(), Some("sonnet"));
        assert_eq!(info.configured_effort.as_deref(), Some("high"));
        // omp's effort is its thinking level; `--effort` means nothing to it.
        let info = choice_agent_info("omp", &args(&["--effort", "high", "--thinking=low"]))
            .expect("omp takes settings");
        assert_eq!(info.configured_model, None);
        assert_eq!(info.configured_effort.as_deref(), Some("low"));
    }

    #[test]
    fn chosen_settings_replace_the_configured_ones_and_keep_the_rest() {
        let configured = args(&["--model", "opus", "--verbose", "--effort=low"]);
        assert_eq!(
            with_settings("claude", &configured, Some("fable"), Some("max")),
            args(&["--verbose", "--model", "fable", "--effort", "max"])
        );
        assert_eq!(
            with_settings("omp", &args(&["--thinking", "low"]), None, Some("xhigh")),
            args(&["--thinking", "xhigh"])
        );
        assert_eq!(with_settings("claude", &configured, None, None), configured);
    }

    #[test]
    fn only_offered_settings_pass_the_check() {
        let info = choice_agent_info("claude", &[]);
        assert!(check(info.as_ref(), Some("opus"), Some("high")).is_ok());
        assert_eq!(
            check(info.as_ref(), Some("gpt"), None).map_err(|(code, _)| code),
            Err("unknown_model")
        );
        assert_eq!(
            check(info.as_ref(), None, Some("extreme")).map_err(|(code, _)| code),
            Err("unknown_effort")
        );
        // A choice that offers nothing takes nothing.
        assert_eq!(
            check(None, Some("opus"), None).map_err(|(code, _)| code),
            Err("unknown_model")
        );
        assert!(check(None, None, None).is_ok());
    }
}
