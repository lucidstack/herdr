//! Optional endpoint companion carrying the server's work items to client shells.
//!
//! Sent as a named `EndpointControl` so the frozen `shell.snapshot.v1` codec is
//! untouched; clients that do not know the kind ignore it.

use serde::{Deserialize, Serialize};

use super::ServerMessage;
use crate::api::schema::{
    WorkItemInfo, WorkItemPickNextInfo, WorkItemProject, WorkItemRepositoryInfo, WorkItemSourceInfo,
};

pub const WORK_ITEMS_PROJECTION_KIND: &str = "endpoint.work-items.v1";

/// Sent alongside (never instead of) the projection, one per notice. Kept out of
/// the frozen `SemanticNotification` wire enum, which an older generation-1
/// endpoint client cannot safely gain new variants on; unaware clients just
/// ignore the kind, so a notice quietly stops arriving until the client updates
/// rather than failing to decode. The client turns this into its own semantic
/// notification locally and picks the sound from `[ui.sound] inbox_path` /
/// `inbox_enabled`.
pub const WORK_ITEMS_NOTICE_KIND: &str = "endpoint.work-items.notice.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointWorkItemsProjection {
    pub boot_id: String,
    pub revision: u64,
    #[serde(default)]
    pub sources: Vec<WorkItemSourceInfo>,
    #[serde(default)]
    pub items: Vec<WorkItemInfo>,
    #[serde(default)]
    pub pick_next: WorkItemPickNextInfo,
    #[serde(default)]
    pub repositories: Vec<WorkItemRepositoryInfo>,
    /// Repositories and tracker projects kept out of this session's inbox.
    #[serde(default)]
    pub ignored_projects: Vec<WorkItemProject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointWorkItemsNotice {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}

pub fn projection_message(
    projection: &EndpointWorkItemsProjection,
) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: WORK_ITEMS_PROJECTION_KIND.into(),
        data: serde_json::to_string(projection)?,
    })
}

pub fn notice_message(title: String, body: Option<String>) -> serde_json::Result<ServerMessage> {
    let notice = EndpointWorkItemsNotice { title, body };
    Ok(ServerMessage::EndpointControl {
        kind: WORK_ITEMS_NOTICE_KIND.into(),
        data: serde_json::to_string(&notice)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_message_round_trips_under_its_kind() {
        let projection = EndpointWorkItemsProjection {
            boot_id: "boot".into(),
            revision: 3,
            sources: vec![WorkItemSourceInfo {
                source_id: "github".into(),
                label: "GitHub".into(),
                error: Some("offline".into()),
            }],
            items: Vec::new(),
            pick_next: WorkItemPickNextInfo::default(),
            repositories: vec![WorkItemRepositoryInfo {
                path: "/src/app".into(),
                label: "app".into(),
                workspace_id: Some("w1".into()),
            }],
            ignored_projects: vec![WorkItemProject {
                source_id: "github".into(),
                project: "o/personal".into(),
            }],
        };
        let ServerMessage::EndpointControl { kind, data } =
            projection_message(&projection).unwrap()
        else {
            panic!("expected endpoint control");
        };
        assert_eq!(kind, WORK_ITEMS_PROJECTION_KIND);
        let decoded: EndpointWorkItemsProjection = serde_json::from_str(&data).unwrap();
        assert_eq!(decoded, projection);
    }
}
