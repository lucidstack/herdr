use crate::api::schema::{
    ResponseResult, ServiceAddParams, ServiceListEntry, ServiceListParams, ServiceLivenessWire,
    ServiceRemoveParams,
};
use crate::service::{find_service_by_label, remove_service_by_id_or_label, Service};
use super::responses;

pub(super) trait ServiceHandlers {
    fn handle_service_add(&mut self, id: String, params: ServiceAddParams) -> String;
    fn handle_service_list(&mut self, id: String, params: ServiceListParams) -> String;
    fn handle_service_remove(&mut self, id: String, params: ServiceRemoveParams) -> String;
}

impl ServiceHandlers for crate::app::App {
    fn handle_service_add(&mut self, id: String, params: ServiceAddParams) -> String {
        // Attribution resolution: workspace_id > pane_id → owning workspace > error
        let workspace_id = match params.workspace_id {
            Some(ws_id) => ws_id,
            None => match params.pane_id {
                Some(pane_id) => {
                    let (_ws_idx, _pane_id) = match self.parse_pane_id(&pane_id) {
                        Some((ws_idx, pane_id)) => (ws_idx, pane_id),
                        None => {
                            return responses::encode_error(
                                id,
                                "pane_not_found",
                                format!("pane {pane_id} not found"),
                            );
                        }
                    };
                    self.public_workspace_id(_ws_idx)
                }
                None => {
                    return responses::encode_error(
                        id,
                        "attribution_required",
                        "workspace_id or pane_id required",
                    );
                }
            },
        };

        let ws_idx = match self.parse_workspace_id(&workspace_id) {
            Some(idx) => idx,
            None => {
                return responses::encode_error(
                    id,
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                );
            }
        };

        // Normalise URL
        let url = match Service::normalise_url(&params.url) {
            Some(normalised) => normalised.into_owned(),
            None => {
                return responses::encode_error(
                    id,
                    "invalid_url",
                    format!("invalid URL: {}", params.url),
                );
            }
        };

        let source = params.source.unwrap_or_else(|| "cli".to_string());
        let workspace = &mut self.state.workspaces[ws_idx];

        // UPSERT: find by label, update or insert
        if let Some(pos) = find_service_by_label(&workspace.services, &params.label) {
            let existing_id = workspace.services[pos].id;
            workspace.services[pos].url = url;
            workspace.services[pos].source = source;
            self.event_tx.blocking_send(crate::AppEvent::ServicesChanged {
                workspace_id: workspace_id.clone(),
            }).ok();
            return responses::encode_success(
                id,
                ResponseResult::ServiceAdd {
                    service_id: existing_id,
                    workspace_id,
                },
            );
        }

        // Insert new service
        let service_id = workspace.next_service_id;
        workspace.next_service_id += 1;
        workspace.services.push(Service::new(
            service_id,
            params.label.clone(),
            url,
            source,
        ));

        self.event_tx.blocking_send(crate::AppEvent::ServicesChanged {
            workspace_id: workspace_id.clone(),
        }).ok();

        responses::encode_success(
            id,
            ResponseResult::ServiceAdd {
                service_id,
                workspace_id,
            },
        )
    }

    fn handle_service_list(&mut self, id: String, params: ServiceListParams) -> String {
        let services = if let Some(ws_id) = params.workspace_id {
            let ws_idx = match self.parse_workspace_id(&ws_id) {
                Some(idx) => idx,
                None => {
                    return responses::encode_error(
                        id,
                        "workspace_not_found",
                        format!("workspace {ws_id} not found"),
                    );
                }
            };
            self.state.workspaces[ws_idx]
                .services
                .iter()
                .map(|svc| ServiceListEntry {
                    workspace_id: ws_id.clone(),
                    id: svc.id,
                    label: svc.label.clone(),
                    url: svc.url.clone(),
                    source: svc.source.clone(),
                    liveness: ServiceLivenessWire::Unknown,
                })
                .collect()
        } else {
            self.state
                .workspaces
                .iter()
                .flat_map(|ws| {
                    ws.services.iter().map(move |svc| ServiceListEntry {
                        workspace_id: ws.id.clone(),
                        id: svc.id,
                        label: svc.label.clone(),
                        url: svc.url.clone(),
                        source: svc.source.clone(),
                        liveness: ServiceLivenessWire::Unknown,
                    })
                })
                .collect()
        };

        responses::encode_success(
            id,
            ResponseResult::ServiceList { services },
        )
    }

    fn handle_service_remove(&mut self, id: String, params: ServiceRemoveParams) -> String {
        let ws_idx = match self.parse_workspace_id(&params.workspace_id) {
            Some(idx) => idx,
            None => {
                return responses::encode_error(
                    id,
                    "workspace_not_found",
                    format!("workspace {} not found", params.workspace_id),
                );
            }
        };

        if params.id.is_none() && params.label.is_none() {
            return responses::encode_error(
                id,
                "invalid_request",
                "id or label required",
            );
        }

        let workspace = &mut self.state.workspaces[ws_idx];
        let removed = remove_service_by_id_or_label(
            &mut workspace.services,
            params.id,
            params.label.as_deref(),
        );

        if !removed {
            return responses::encode_error(
                id,
                "service_not_found",
                "service not found",
            );
        }

        self.event_tx.blocking_send(crate::AppEvent::ServicesChanged {
            workspace_id: params.workspace_id.clone(),
        }).ok();

        responses::encode_success(id, ResponseResult::ServiceRemove {})
    }
}
