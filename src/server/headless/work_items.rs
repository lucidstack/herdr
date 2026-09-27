//! Work-item events and projection delivery for client shells.

use crate::app::App;
use crate::events::AppEvent;

use super::HeadlessServer;

impl HeadlessServer {
    /// Applies a work-item event and announces new arrivals to every client shell.
    pub(super) fn handle_work_items_app_event(&mut self, ev: AppEvent) -> bool {
        let AppEvent::WorkItems(event) = ev else {
            return self.app.handle_internal_event_with_render_impact(ev);
        };
        let (changed, notices) = self.app.handle_work_items_event(*event);
        self.send_work_item_notices(notices);
        self.follow_work_items_focus();
        changed
    }

    /// Runs periodic work-item tasks and delivers any notices they produced.
    pub(super) fn run_work_items_tasks_headless(&mut self, now: std::time::Instant) -> bool {
        let changed = self.app.run_work_items_tasks(now);
        let notices = self.app.work_items.take_notices();
        self.send_work_item_notices(notices);
        self.follow_work_items_focus();
        changed
    }

    /// Work items focused a workspace outside an API request (a new "Pick next"
    /// workspace): move shell clients there, as a public `workspace.focus` would.
    fn follow_work_items_focus(&mut self) {
        if self.app.work_items.take_focus_request() {
            self.focus_all_shell_clients_on_default_target();
        }
    }

    fn send_work_item_notices(&mut self, notices: Vec<crate::work_items::WorkItemNotice>) {
        for notice in notices {
            match crate::protocol::work_items::notice_message(notice.title, notice.body) {
                Ok(message) => {
                    self.send_to_client_shells(message);
                }
                Err(err) => {
                    tracing::warn!(err = %err, "failed to encode work item notice");
                }
            }
        }
    }

    /// Frames the current work-item projection for one client shell.
    pub(super) fn frame_work_items_projection(app: &App, boot_id: &str) -> Result<Vec<u8>, String> {
        let message = crate::protocol::work_items::projection_message(
            boot_id,
            app.work_items.revision(),
            app.work_items.source_infos(),
            app.work_items.projection_items(),
            app.work_items.pick_next_info(),
        )
        .map_err(|err| err.to_string())?;
        Self::frame_server_message(&message).map_err(|err| err.to_string())
    }
}
