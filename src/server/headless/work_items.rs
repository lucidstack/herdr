//! Work-item events and projection delivery for client shells.

use crate::app::App;
use crate::events::AppEvent;
use crate::protocol::{
    SemanticNotification, SemanticNotificationKind, SemanticNotificationSound, ServerMessage,
};

use super::HeadlessServer;

impl HeadlessServer {
    /// Applies a work-item event and announces new arrivals to every client shell.
    pub(super) fn handle_work_items_app_event(&mut self, ev: AppEvent) -> bool {
        let AppEvent::WorkItems(event) = ev else {
            return self.app.handle_internal_event_with_render_impact(ev);
        };
        let (changed, notices) = self.app.handle_work_items_event(*event);
        for notice in notices {
            self.send_to_client_shells(ServerMessage::SemanticNotification(SemanticNotification {
                kind: SemanticNotificationKind::Custom,
                title: notice.title,
                body: notice.body,
                sound: Some(SemanticNotificationSound::Request),
                agent: None,
                workspace_id: None,
                tab_id: None,
                pane_id: None,
                position: None,
            }));
        }
        changed
    }

    /// Frames the current work-item projection for one client shell.
    pub(super) fn frame_work_items_projection(app: &App, boot_id: &str) -> Result<Vec<u8>, String> {
        let message = crate::protocol::work_items::projection_message(
            boot_id,
            app.work_items.revision(),
            app.work_items.source_infos(),
            app.work_items.projection_items(),
        )
        .map_err(|err| err.to_string())?;
        Self::frame_server_message(&message).map_err(|err| err.to_string())
    }
}
