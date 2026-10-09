use crate::api::schema::{
    ResponseResult, WorkItemChoiceAction, WorkItemChooseParams, WorkItemCreateParams,
    WorkItemHideParams, WorkItemLinkParams, WorkItemPickNextStartParams, WorkItemProject,
    WorkItemTarget,
};
use crate::app::App;
use crate::work_items::source::{LOCAL_DONE_CHOICE_ID, LOCAL_REOPEN_CHOICE_ID};

use super::responses::{encode_error, encode_success};

const DISABLED_CODE: &str = "work_items_disabled";
const DISABLED_MESSAGE: &str = "no work item sources are configured";
const NOT_FOUND_CODE: &str = "work_item_not_found";

fn not_found(id: String, item_id: &str) -> String {
    encode_error(id, NOT_FOUND_CODE, format!("unknown work item {item_id}"))
}

impl App {
    pub(super) fn handle_work_item_list(&mut self, id: String) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        encode_success(
            id,
            ResponseResult::WorkItemList {
                items: self.work_items.projection_items(),
                sources: self.work_items.source_infos(),
                pick_next: self.work_items.pick_next_info(),
                repositories: self.work_items.repository_infos(),
                agents: self.agents_outside_items(),
                ignored_projects: self.work_items.ignored_projects().to_vec(),
            },
        )
    }

    pub(super) fn handle_work_item_mark_seen(
        &mut self,
        id: String,
        params: WorkItemTarget,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        match self.work_items.mark_seen(&params.item_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(_) => not_found(id, &params.item_id),
        }
    }

    pub(super) fn handle_work_item_choose(
        &mut self,
        id: String,
        params: WorkItemChooseParams,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let Some(item) = self.work_items.get(&params.item_id) else {
            return not_found(id, &params.item_id);
        };
        let is_local = item.is_local();
        let Some(choice) = self
            .work_items
            .item_choices(item)
            .choices
            .into_iter()
            .find(|choice| choice.choice_id == params.choice_id)
        else {
            return encode_error(
                id,
                "work_item_choice_not_found",
                format!("unknown choice {} for {}", params.choice_id, params.item_id),
            );
        };
        // Checked whatever the choice does, so a misspelt option never runs it with defaults.
        // Only a choice that starts a workspace takes its options into account so far.
        let options = match crate::work_items::source::switched_on_options(
            &choice,
            params.options.as_deref(),
        ) {
            Ok(options) => options,
            Err(unknown) => {
                return encode_error(
                    id,
                    "unknown_option",
                    format!(
                        "unknown option {unknown} for choice {} of {}",
                        params.choice_id, params.item_id
                    ),
                );
            }
        };
        // Checked like the options: a misspelt model never starts the agent as configured.
        let model = params.model.as_deref();
        let effort = params.effort.as_deref();
        if let Err((code, message)) =
            crate::work_items::agent_settings::check(choice.agent.as_ref(), model, effort)
        {
            return encode_error(
                id,
                code,
                format!(
                    "{message} for choice {} of {}",
                    params.choice_id, params.item_id
                ),
            );
        }
        if choice.choice_id == crate::work_items::source::MUTE_START_REMINDER_CHOICE_ID {
            return match self.work_items.mute_start_reminder(&params.item_id) {
                Ok(()) => encode_success(id, ResponseResult::Ok {}),
                Err(_) => not_found(id, &params.item_id),
            };
        }
        // Herdr's own choices for a local item, which no source knows.
        if is_local {
            let resolved = match choice.choice_id.as_str() {
                LOCAL_DONE_CHOICE_ID => Some(true),
                LOCAL_REOPEN_CHOICE_ID => Some(false),
                _ => None,
            };
            if let Some(resolved) = resolved {
                return match self
                    .work_items
                    .set_local_resolved(&params.item_id, resolved)
                {
                    Ok(()) => encode_success(id, ResponseResult::Ok {}),
                    Err(_) => not_found(id, &params.item_id),
                };
            }
        }
        if let Some(reason) = choice.disabled_reason {
            return encode_error(id, "work_item_choice_unavailable", reason);
        }
        match choice.action {
            // The client owns the browser and opens the URL itself.
            WorkItemChoiceAction::OpenUrl { .. } => {
                match self.work_items.set_awaiting_external(&params.item_id) {
                    Ok(()) => encode_success(id, ResponseResult::Ok {}),
                    Err(_) => not_found(id, &params.item_id),
                }
            }
            WorkItemChoiceAction::ProvisionWorkspace => {
                match self.start_work_item_provisioning(
                    &params.item_id,
                    &params.choice_id,
                    &options,
                    (model, effort),
                ) {
                    Ok(()) => encode_success(id, ResponseResult::Ok {}),
                    Err((code, message)) => encode_error(id, code, message),
                }
            }
            WorkItemChoiceAction::Perform => {
                match self.start_work_item_action(&params.item_id, &params.choice_id) {
                    Ok(()) => encode_success(id, ResponseResult::Ok {}),
                    Err((code, message)) => encode_error(id, code, message),
                }
            }
            WorkItemChoiceAction::BriefAgent => {
                match self.start_work_item_follow_up(&params.item_id, &params.choice_id) {
                    Ok(()) => encode_success(id, ResponseResult::Ok {}),
                    Err((code, message)) => encode_error(id, code, message),
                }
            }
            WorkItemChoiceAction::Unknown => encode_error(
                id,
                "work_item_choice_unavailable",
                format!("choice {} is not supported", params.choice_id),
            ),
        }
    }

    pub(super) fn handle_work_item_hide(
        &mut self,
        id: String,
        params: WorkItemHideParams,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let until = params
            .snooze_seconds
            .map(|seconds| crate::work_items::unix_now().saturating_add(seconds.max(1)));
        match self.work_items.hide(&params.item_id, until) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(_) => not_found(id, &params.item_id),
        }
    }

    pub(super) fn handle_work_item_link(
        &mut self,
        id: String,
        params: WorkItemLinkParams,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let Some(workspace_id) = self.work_item_workspace_id(&params.workspace_id) else {
            return encode_error(
                id,
                "workspace_not_found",
                format!("unknown workspace {}", params.workspace_id),
            );
        };
        match self.work_items.link(&params.item_id, &workspace_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(_) => not_found(id, &params.item_id),
        }
    }

    /// The stable public id of the open workspace `requested` names, in any form
    /// `work_item.link` takes: its public id, `w_<n>` or the number. `None` when there is no
    /// such workspace; the numeric forms are not looked up among the open ones, so a number
    /// past the last workspace is checked here.
    fn work_item_workspace_id(&self, requested: &str) -> Option<String> {
        let ws_idx = self
            .parse_workspace_id(requested)
            .filter(|ws_idx| *ws_idx < self.state.workspaces.len())?;
        // Stored as the stable public id, whatever form the caller used.
        Some(self.public_workspace_id(ws_idx))
    }

    pub(super) fn handle_work_item_create(
        &mut self,
        id: String,
        params: WorkItemCreateParams,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        let title = params.title.trim();
        if title.is_empty() {
            return encode_error(id, "invalid_title", "title must not be empty");
        }
        let workspace_id = match params.workspace_id.as_deref() {
            None => None,
            Some(requested) => match self.work_item_workspace_id(requested) {
                Some(workspace_id) => Some(workspace_id),
                None => {
                    return encode_error(
                        id,
                        "workspace_not_found",
                        format!("unknown workspace {requested}"),
                    );
                }
            },
        };
        let key = self.work_items.create_local(title, workspace_id.as_deref());
        match self.work_items.item_info(&key) {
            Some(item) => encode_success(id, ResponseResult::WorkItemAdded { item }),
            None => not_found(id, &key),
        }
    }

    pub(super) fn handle_work_item_unhide(&mut self, id: String, params: WorkItemTarget) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        match self.work_items.unhide(&params.item_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(_) => not_found(id, &params.item_id),
        }
    }

    pub(super) fn handle_work_item_pick_next_start(
        &mut self,
        id: String,
        params: WorkItemPickNextStartParams,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        match self.start_pick_next(&params.source_id, &params.context) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err((code, message)) => encode_error(id, code, message),
        }
    }

    pub(super) fn handle_work_item_ignore_project(
        &mut self,
        id: String,
        params: WorkItemProject,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        match self.work_items.ignore_project(&params) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err((code, message)) => encode_error(id, code, message),
        }
    }

    pub(super) fn handle_work_item_unignore_project(
        &mut self,
        id: String,
        params: WorkItemProject,
    ) -> String {
        if !self.work_items.is_enabled() {
            return encode_error(id, DISABLED_CODE, DISABLED_MESSAGE);
        }
        self.work_items.unignore_project(&params);
        encode_success(id, ResponseResult::Ok {})
    }
}
