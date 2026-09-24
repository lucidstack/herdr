use crate::api::schema::{
    ResponseResult, WorkItemChoiceAction, WorkItemChooseParams, WorkItemTarget,
};
use crate::app::App;

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
        let Some(source) = self.work_items.source(&item.source_id) else {
            return not_found(id, &params.item_id);
        };
        let Some(choice) = source
            .choices(item)
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
                match self.start_work_item_provisioning(&params.item_id, &params.choice_id) {
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
}
