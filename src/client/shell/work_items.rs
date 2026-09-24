//! Client-side copy of each endpoint's work-item projection.

use std::collections::HashMap;

use crate::client::endpoint::ClientEndpointId;
use crate::protocol::work_items::EndpointWorkItemsProjection;

use super::ClientShellState;

#[derive(Default)]
pub(crate) struct ClientWorkItems {
    by_endpoint: HashMap<ClientEndpointId, EndpointWorkItemsProjection>,
}

impl ClientWorkItems {
    pub(super) fn remove_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        self.by_endpoint.remove(endpoint_id);
    }

    /// Stores `projection` when it is newer than the current one; returns whether it was stored.
    fn store(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: EndpointWorkItemsProjection,
    ) -> bool {
        let newer = self.by_endpoint.get(endpoint_id).is_none_or(|current| {
            current.boot_id != projection.boot_id || projection.revision > current.revision
        });
        if newer {
            self.by_endpoint.insert(endpoint_id.clone(), projection);
        }
        newer
    }
}

impl ClientShellState {
    pub(crate) fn set_endpoint_work_items(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: EndpointWorkItemsProjection,
    ) -> bool {
        self.work_items.store(endpoint_id, projection)
    }
}
