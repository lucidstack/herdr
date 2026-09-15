use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, ServiceAddParams, ServiceListEntry,
    ServiceListParams, ServiceLivenessWire, ServiceRemoveParams,
};
use crate::service::{find_service_by_label, remove_service_by_id_or_label, Service};
use super::responses;

pub(super) trait ServiceHandlers {
    fn handle_service_add(&mut self, id: String, params: ServiceAddParams) -> String;
    fn handle_service_list(&mut self, id: String, params: ServiceListParams) -> String;
    fn handle_service_remove(&mut self, id: String, params: ServiceRemoveParams) -> String;
}

/// Resolve workspace_id from optional workspace_id or pane_id; attribution_required if neither available
fn resolve_workspace_id(app: &crate::app::App, request_id: &str, workspace_id: Option<String>, pane_id: Option<String>) -> Result<String, String> {
    match workspace_id {
        Some(ws_id) => Ok(ws_id),
        None => match pane_id {
            Some(pane_id) => {
                let (_ws_idx, _pane_id) = match app.parse_pane_id(&pane_id) {
                    Some((ws_idx, pane_id)) => (ws_idx, pane_id),
                    None => {
                        return Err(responses::encode_error(
                            request_id.to_string(),
                            "pane_not_found",
                            format!("pane {pane_id} not found"),
                        ));
                    }
                };
                Ok(app.public_workspace_id(_ws_idx))
            }
            None => {
                Err(responses::encode_error(
                    request_id.to_string(),
                    "attribution_required",
                    "workspace_id or pane_id required",
                ))
            }
        },
    }
}

impl ServiceHandlers for crate::app::App {
    fn handle_service_add(&mut self, id: String, params: ServiceAddParams) -> String {
        let workspace_id = match resolve_workspace_id(self, &id, params.workspace_id, params.pane_id) {
            Ok(ws_id) => ws_id,
            Err(err_response) => return err_response,
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
            let services = self.build_service_list_for_workspace(ws_idx);
            self.emit_event(EventEnvelope {
                event: EventKind::ServicesChanged,
                data: EventData::ServicesChanged {
                    workspace_id: workspace_id.clone(),
                    services,
                },
            });
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

        let services = self.build_service_list_for_workspace(ws_idx);
        self.emit_event(EventEnvelope {
            event: EventKind::ServicesChanged,
            data: EventData::ServicesChanged {
                workspace_id: workspace_id.clone(),
                services,
            },
        });

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
            self.build_service_list_for_workspace(ws_idx)
        } else {
            self.state
                .workspaces
                .iter()
                .enumerate()
                .flat_map(|(_, ws)| self.build_service_list_for_workspace_ref(ws))
                .collect()
        };

        responses::encode_success(
            id,
            ResponseResult::ServiceList { services },
        )
    }

    fn handle_service_remove(&mut self, id: String, params: ServiceRemoveParams) -> String {
        let workspace_id = match resolve_workspace_id(self, &id, params.workspace_id, params.pane_id) {
            Ok(ws_id) => ws_id,
            Err(err_response) => return err_response,
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

        let services = self.build_service_list_for_workspace(ws_idx);
        self.emit_event(EventEnvelope {
            event: EventKind::ServicesChanged,
            data: EventData::ServicesChanged {
                workspace_id: workspace_id.clone(),
                services,
            },
        });

        responses::encode_success(id, ResponseResult::ServiceRemove {})
    }
}

impl crate::app::App {
    fn build_service_list_for_workspace(&self, ws_idx: usize) -> Vec<ServiceListEntry> {
        if let Some(ws) = self.state.workspaces.get(ws_idx) {
            self.build_service_list_for_workspace_ref(ws)
        } else {
            Vec::new()
        }
    }

    fn build_service_list_for_workspace_ref(
        &self,
        ws: &crate::workspace::Workspace,
    ) -> Vec<ServiceListEntry> {
        ws.services
            .iter()
            .map(|svc| ServiceListEntry {
                workspace_id: ws.id.clone(),
                id: svc.id,
                label: svc.label.clone(),
                url: svc.url.clone(),
                source: svc.source.clone(),
                liveness: ServiceLivenessWire::from(svc.liveness),
            })
            .collect()
    }
}
