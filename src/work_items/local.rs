//! Local items: work named by hand, with no tracker behind it. No source knows them, so
//! Herdr offers their choices itself: starting work in one of the mapped repositories or in
//! a scratch folder, and marking them done.

use std::path::Path;

use crate::api::schema::{WorkItemChoiceAction, WorkItemChoiceInfo};
use crate::config::AgentLaunch;

use super::source::{
    ItemChoices, ProvisionPlan, WorkspaceLayout, WorkspaceSource, WorktreeSpec,
    LOCAL_DONE_CHOICE_ID, LOCAL_REOPEN_CHOICE_ID,
};
use super::state::WorkItem;
use super::Repository;

/// Starts work in a mapped repository: the rest of the id is the clone's path.
const START_IN_PREFIX: &str = "start_in:";
/// Starts work in a scratch folder, with no checkout.
const START_SCRATCH_CHOICE_ID: &str = "start_scratch";
const MAX_LABEL_CHARS: usize = 40;

/// What a local item offers: while it has no workspace and is not done, starting work in each
/// mapped repository and in a scratch folder; then marking it done, or taking that back. None
/// is a default.
pub(crate) fn choices(item: &WorkItem, repositories: &[Repository]) -> ItemChoices {
    let start = |choice_id: String, label: String, description: String| WorkItemChoiceInfo {
        choice_id,
        label,
        description: Some(description),
        action: WorkItemChoiceAction::ProvisionWorkspace,
        disabled_reason: None,
        confirm: None,
        options: Vec::new(),
        agent: None,
    };
    let mut choices = Vec::new();
    if item.workspace_id.is_none() && !item.resolved {
        choices.extend(repositories.iter().map(|repository| {
            start(
                format!("{START_IN_PREFIX}{}", repository.info.path),
                format!("Start in {}", repository.info.label),
                format!("A worktree of {} on a new branch", repository.info.path),
            )
        }));
        choices.push(start(
            START_SCRATCH_CHOICE_ID.into(),
            "Start in a scratch folder".into(),
            "An empty folder, with no checkout".into(),
        ));
    }
    let (choice_id, label) = if item.resolved {
        (LOCAL_REOPEN_CHOICE_ID, "Not done yet")
    } else {
        (LOCAL_DONE_CHOICE_ID, "Mark as done")
    };
    choices.push(WorkItemChoiceInfo {
        choice_id: choice_id.into(),
        label: label.into(),
        description: None,
        action: WorkItemChoiceAction::Perform,
        disabled_reason: None,
        confirm: None,
        options: Vec::new(),
        agent: None,
    });
    ItemChoices {
        choices,
        default_choice_id: None,
    }
}

/// The workspace `choice_id` starts for `item`: a worktree of the chosen repository on a new
/// branch named after the title, or a scratch folder. `agent` starts in it with no brief, so
/// you write the first prompt yourself.
pub(crate) fn provision_plan(
    item: &WorkItem,
    choice_id: &str,
    repositories: &[Repository],
    worktree_directory: &Path,
    agent: &AgentLaunch,
) -> Result<ProvisionPlan, String> {
    let number = item
        .key
        .rsplit(':')
        .next()
        .unwrap_or(item.key.as_str())
        .to_string();
    let slug = super::github::slug(&item.title);
    let name = if slug.is_empty() {
        format!("local-{number}")
    } else {
        format!("local-{number}-{slug}")
    };
    let (source, tabs) = if choice_id == START_SCRATCH_CHOICE_ID {
        let directory = crate::worktree::default_checkout_path(worktree_directory, "local", &name);
        (WorkspaceSource::Scratch(directory), Vec::new())
    } else {
        let path = choice_id
            .strip_prefix(START_IN_PREFIX)
            .ok_or_else(|| format!("choice {choice_id} does not provision a workspace"))?;
        let repository = repositories
            .iter()
            .find(|repository| repository.info.path == path)
            .ok_or_else(|| format!("{path} is no longer a mapped repository"))?;
        // A private ref as base keeps the new branch from tracking the base branch.
        let (fetched, base) = match repository.base_branch.as_deref() {
            Some(base) => (format!("refs/heads/{base}"), base),
            None => ("HEAD".to_string(), "HEAD"),
        };
        (
            WorkspaceSource::Worktree(WorktreeSpec {
                repo_path: crate::worktree::expand_tilde_absolute_path(path),
                remote: repository.remote.clone(),
                fetch_refspec: format!("+{fetched}:refs/herdr/base/{base}"),
                base_ref: format!("refs/herdr/base/{base}"),
                branch: name.clone(),
                reuse_branch: true,
                extra_fetch_refspecs: Vec::new(),
                adopt_branch_for: None,
            }),
            crate::config::default_branch_tabs(),
        )
    };
    Ok(ProvisionPlan {
        source,
        workspace_label: super::github::truncate_chars(&item.title, MAX_LABEL_CHARS),
        agent_name_hint: format!("local-{number}"),
        brief: String::new(),
        plan_command: String::new(),
        layout: WorkspaceLayout {
            agent: agent.agent.clone(),
            agent_args: agent.args.clone(),
            tabs,
            diff_command: String::new(),
        },
        delete_branch: false,
        shared_with: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::api::schema::WorkItemRepositoryInfo;

    fn repository(path: &str, base_branch: Option<&str>) -> Repository {
        Repository {
            info: WorkItemRepositoryInfo {
                path: path.into(),
                label: path.rsplit('/').next().unwrap_or(path).into(),
                workspace_id: None,
            },
            repo_key: None,
            remote: "origin".into(),
            base_branch: base_branch.map(str::to_string),
        }
    }

    fn local(title: &str) -> WorkItem {
        let mut state = super::super::state::WorkItemsState::default();
        let key = state.create_local(title, None, "2026-10-14T00:00:00Z".into());
        state.get(&key).cloned().expect("local item")
    }

    fn ids(choices: &ItemChoices) -> Vec<&str> {
        choices
            .choices
            .iter()
            .map(|choice| choice.choice_id.as_str())
            .collect()
    }

    #[test]
    fn a_new_local_item_starts_in_each_repository_or_a_scratch_folder() {
        let repositories = [repository("/src/app", None), repository("/src/api", None)];
        let mut item = local("Answer the Teams request");
        assert_eq!(
            ids(&choices(&item, &repositories)),
            [
                "start_in:/src/app",
                "start_in:/src/api",
                "start_scratch",
                "done"
            ]
        );
        // Once work has started, or is done, there is nothing to start.
        item.workspace_id = Some("w1".into());
        assert_eq!(ids(&choices(&item, &repositories)), ["done"]);
        item.workspace_id = None;
        item.resolved = true;
        assert_eq!(ids(&choices(&item, &repositories)), ["reopen"]);
    }

    #[test]
    fn a_repository_start_branches_from_its_base_and_starts_the_agent_unbriefed() {
        let item = local("Answer the Teams request");
        let agent = AgentLaunch {
            agent: "omp".into(),
            args: vec!["--fast".into()],
        };
        let plan = |repositories: &[Repository]| {
            provision_plan(
                &item,
                "start_in:/src/app",
                repositories,
                Path::new("/wt"),
                &agent,
            )
            .expect("plan")
        };
        let WorkspaceSource::Worktree(spec) = plan(&[repository("/src/app", None)]).source else {
            panic!("a worktree");
        };
        assert_eq!(spec.repo_path, PathBuf::from("/src/app"));
        assert_eq!(spec.fetch_refspec, "+HEAD:refs/herdr/base/HEAD");
        assert_eq!(spec.base_ref, "refs/herdr/base/HEAD");
        assert_eq!(spec.branch, "local-1-answer-the-teams-request");

        let configured = plan(&[repository("/src/app", Some("develop"))]);
        let WorkspaceSource::Worktree(spec) = &configured.source else {
            panic!("a worktree");
        };
        assert_eq!(
            spec.fetch_refspec,
            "+refs/heads/develop:refs/herdr/base/develop"
        );
        assert!(configured.brief.is_empty() && configured.plan_command.is_empty());
        assert_eq!(
            (
                configured.layout.agent.as_str(),
                configured.layout.agent_args.as_slice()
            ),
            ("omp", ["--fast".to_string()].as_slice())
        );

        assert!(
            provision_plan(&item, "start_in:/src/gone", &[], Path::new("/wt"), &agent).is_err(),
            "a repository no longer mapped starts nothing"
        );
    }

    #[test]
    fn a_scratch_start_has_no_checkout() {
        let plan = provision_plan(
            &local("Answer the Teams request"),
            "start_scratch",
            &[],
            Path::new("/wt"),
            &AgentLaunch::default(),
        )
        .expect("plan");
        assert!(matches!(plan.source, WorkspaceSource::Scratch(_)));
        assert!(plan.layout.tabs.is_empty());
    }
}
