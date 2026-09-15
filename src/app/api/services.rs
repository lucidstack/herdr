use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, ServiceAddParams, ServiceListEntry,
    ServiceListParams, ServiceRemoveParams,
};
use crate::app::App;
use crate::service::{normalise_url, Service};

use super::responses::{encode_error, encode_success};

const DEFAULT_SERVICE_SOURCE: &str = "cli";

impl App {
    pub(super) fn handle_service_add(&mut self, id: String, params: ServiceAddParams) -> String {
        let index = match self.resolve_service_workspace(params.workspace_id, params.pane_id) {
            Ok(index) => index,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let label = params.label.trim();
        if label.is_empty() {
            return encode_error(id, "invalid_label", "service label must not be empty");
        }
        let Some(url) = normalise_url(&params.url) else {
            return encode_error(
                id,
                "invalid_url",
                format!(
                    "service url {:?} must be an http(s) URL, host:port, or :port",
                    params.url
                ),
            );
        };
        let source = params
            .source
            .map(|source| source.trim().to_string())
            .filter(|source| !source.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_SOURCE.to_string());
        let workspace_id = self.public_workspace_id(index);
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &workspace_id);
        };

        let (service_id, changed) = match workspace
            .services
            .iter_mut()
            .find(|service| service.label == label)
        {
            Some(existing) => {
                let changed = existing.url != url || existing.source != source;
                if existing.url != url {
                    existing.url = url;
                    existing.liveness = crate::service::ServiceLiveness::Unknown;
                }
                existing.source = source;
                (existing.id, changed)
            }
            None => {
                let service_id = workspace.next_service_id;
                workspace.next_service_id += 1;
                workspace
                    .services
                    .push(Service::new(service_id, label, url, source));
                (service_id, true)
            }
        };

        if changed {
            self.emit_services_changed(index);
        }
        encode_success(
            id,
            ResponseResult::ServiceAdded {
                service_id,
                workspace_id,
            },
        )
    }

    pub(super) fn handle_service_list(&mut self, id: String, params: ServiceListParams) -> String {
        let services = match params.workspace_id {
            Some(workspace_id) => {
                let Some(index) = self.parse_workspace_id(&workspace_id) else {
                    return workspace_not_found(id, &workspace_id);
                };
                self.service_list_entries(index)
            }
            None => (0..self.state.workspaces.len())
                .flat_map(|index| self.service_list_entries(index))
                .collect(),
        };
        encode_success(id, ResponseResult::ServiceList { services })
    }

    pub(super) fn handle_service_remove(
        &mut self,
        id: String,
        params: ServiceRemoveParams,
    ) -> String {
        let index = match self.resolve_service_workspace(params.workspace_id, params.pane_id) {
            Ok(index) => index,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let workspace_id = self.public_workspace_id(index);
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &workspace_id);
        };
        let position = match (params.id, params.label.as_deref()) {
            (Some(service_id), _) => workspace
                .services
                .iter()
                .position(|service| service.id == service_id),
            (None, Some(label)) => workspace
                .services
                .iter()
                .position(|service| service.label == label.trim()),
            (None, None) => {
                return encode_error(
                    id,
                    "service_selector_required",
                    "service.remove requires id or label",
                );
            }
        };
        let Some(position) = position else {
            return encode_error(
                id,
                "service_not_found",
                format!("no matching service in workspace {workspace_id}"),
            );
        };
        workspace.services.remove(position);
        self.emit_services_changed(index);
        encode_success(id, ResponseResult::Ok {})
    }

    /// Resolves the owning workspace for a service request: an explicit
    /// `workspace_id` wins, otherwise the workspace that owns `pane_id`.
    fn resolve_service_workspace(
        &self,
        workspace_id: Option<String>,
        pane_id: Option<String>,
    ) -> Result<usize, (&'static str, String)> {
        if let Some(workspace_id) = workspace_id {
            return self.parse_workspace_id(&workspace_id).ok_or_else(|| {
                (
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                )
            });
        }
        if let Some(pane_id) = pane_id {
            return self
                .parse_pane_id(&pane_id)
                .map(|(workspace_index, _)| workspace_index)
                .ok_or_else(|| ("pane_not_found", format!("pane {pane_id} not found")));
        }
        Err((
            "attribution_required",
            "workspace_id or pane_id is required".to_string(),
        ))
    }

    pub(crate) fn service_list_entries(&self, index: usize) -> Vec<ServiceListEntry> {
        let Some(workspace) = self.state.workspaces.get(index) else {
            return Vec::new();
        };
        let workspace_id = self.public_workspace_id(index);
        workspace
            .services
            .iter()
            .map(|service| ServiceListEntry {
                workspace_id: workspace_id.clone(),
                id: service.id,
                label: service.label.clone(),
                url: service.url.clone(),
                source: service.source.clone(),
                liveness: service.liveness.into(),
            })
            .collect()
    }

    fn emit_services_changed(&mut self, index: usize) {
        let workspace_id = self.public_workspace_id(index);
        let services = self.service_list_entries(index);
        self.emit_event(EventEnvelope {
            event: EventKind::ServicesChanged,
            data: EventData::ServicesChanged {
                workspace_id,
                services,
            },
        });
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request, ServiceLivenessWire};
    use crate::app::service_liveness::ServiceLivenessProbeResult;
    use crate::config::Config;
    use crate::service::ServiceLiveness;
    use crate::workspace::Workspace;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    fn call(app: &mut App, method: Method) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        serde_json::from_str(&response).expect("json response")
    }

    fn add(app: &mut App, params: ServiceAddParams) -> serde_json::Value {
        call(app, Method::ServiceAdd(params))
    }

    fn add_params(workspace_id: &str, label: &str, url: &str) -> ServiceAddParams {
        ServiceAddParams {
            workspace_id: Some(workspace_id.into()),
            pane_id: None,
            label: label.into(),
            url: url.into(),
            source: None,
        }
    }

    fn listed(app: &mut App, workspace_id: Option<&str>) -> Vec<ServiceListEntry> {
        let response = call(
            app,
            Method::ServiceList(ServiceListParams {
                workspace_id: workspace_id.map(str::to_string),
            }),
        );
        serde_json::from_value(response["result"]["services"].clone()).expect("service list")
    }

    #[test]
    fn add_is_an_upsert_keyed_by_label() {
        let mut app = test_app();
        let workspace_id = app.state.workspaces[0].id.clone();
        let first = add(&mut app, add_params(&workspace_id, "Rails", ":3000"));
        assert_eq!(first["result"]["service_id"], 1);
        app.state.workspaces[0].services[0].liveness = ServiceLiveness::Up;

        let again = add(
            &mut app,
            add_params(&workspace_id, "Rails", "localhost:3001"),
        );
        assert_eq!(again["result"]["service_id"], 1);
        let services = listed(&mut app, Some(&workspace_id));
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].url, "http://localhost:3001");
        assert_eq!(
            services[0].liveness,
            ServiceLivenessWire::Unknown,
            "a changed url invalidates the previous liveness"
        );

        let other = add(&mut app, add_params(&workspace_id, "Vite", ":5173"));
        assert_eq!(other["result"]["service_id"], 2);
    }

    #[test]
    fn explicit_workspace_wins_over_pane_and_missing_both_is_rejected() {
        let mut app = test_app();
        let first_id = app.state.workspaces[0].id.clone();
        let second_id = app.state.workspaces[1].id.clone();
        let second_pane = app.state.workspaces[1].tabs[0].layout.pane_ids()[0];
        let second_pane_id = app.public_pane_id(1, second_pane).expect("public pane id");

        let via_pane = add(
            &mut app,
            ServiceAddParams {
                workspace_id: None,
                pane_id: Some(second_pane_id.clone()),
                label: "Rails".into(),
                url: ":3000".into(),
                source: Some("agent".into()),
            },
        );
        assert_eq!(via_pane["result"]["workspace_id"], second_id);

        let explicit = add(
            &mut app,
            ServiceAddParams {
                workspace_id: Some(first_id.clone()),
                pane_id: Some(second_pane_id),
                label: "Rails".into(),
                url: ":3000".into(),
                source: None,
            },
        );
        assert_eq!(explicit["result"]["workspace_id"], first_id);

        let neither = add(
            &mut app,
            ServiceAddParams {
                workspace_id: None,
                pane_id: None,
                label: "Rails".into(),
                url: ":3000".into(),
                source: None,
            },
        );
        assert_eq!(neither["error"]["code"], "attribution_required");

        let all = listed(&mut app, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].source, "agent");
    }

    #[test]
    fn add_rejects_unusable_input() {
        let mut app = test_app();
        let workspace_id = app.state.workspaces[0].id.clone();
        let bad_url = add(&mut app, add_params(&workspace_id, "Rails", "rails server"));
        assert_eq!(bad_url["error"]["code"], "invalid_url");
        let blank = add(&mut app, add_params(&workspace_id, "  ", ":3000"));
        assert_eq!(blank["error"]["code"], "invalid_label");
        let unknown = add(&mut app, add_params("w999", "Rails", ":3000"));
        assert_eq!(unknown["error"]["code"], "workspace_not_found");
        assert!(app.state.workspaces[0].services.is_empty());
    }

    #[test]
    fn remove_by_id_or_label_and_ids_are_never_reused() {
        let mut app = test_app();
        let workspace_id = app.state.workspaces[0].id.clone();
        add(&mut app, add_params(&workspace_id, "Rails", ":3000"));
        add(&mut app, add_params(&workspace_id, "Vite", ":5173"));

        let remove = |app: &mut App, id: Option<u64>, label: Option<&str>| {
            call(
                app,
                Method::ServiceRemove(ServiceRemoveParams {
                    workspace_id: Some(workspace_id.clone()),
                    pane_id: None,
                    id,
                    label: label.map(str::to_string),
                }),
            )
        };
        assert_eq!(remove(&mut app, Some(1), None)["result"]["type"], "ok");
        assert_eq!(remove(&mut app, None, Some("Vite"))["result"]["type"], "ok");
        assert!(listed(&mut app, Some(&workspace_id)).is_empty());
        assert_eq!(
            remove(&mut app, Some(1), None)["error"]["code"],
            "service_not_found"
        );
        assert_eq!(
            remove(&mut app, None, None)["error"]["code"],
            "service_selector_required"
        );

        let readded = add(&mut app, add_params(&workspace_id, "Rails", ":3000"));
        assert_eq!(readded["result"]["service_id"], 3);
    }

    #[test]
    fn probe_results_update_liveness_and_ignore_stale_targets() {
        let mut app = test_app();
        let workspace_id = app.state.workspaces[0].id.clone();
        add(&mut app, add_params(&workspace_id, "Rails", ":3000"));

        let changed = app.state.apply_service_liveness(vec![
            ServiceLivenessProbeResult {
                workspace_id: workspace_id.clone(),
                service_id: 1,
                liveness: ServiceLiveness::Up,
            },
            ServiceLivenessProbeResult {
                workspace_id: workspace_id.clone(),
                service_id: 42,
                liveness: ServiceLiveness::Up,
            },
            ServiceLivenessProbeResult {
                workspace_id: "w999".into(),
                service_id: 1,
                liveness: ServiceLiveness::Up,
            },
        ]);
        assert!(changed);
        assert_eq!(
            listed(&mut app, Some(&workspace_id))[0].liveness,
            ServiceLivenessWire::Up
        );

        let unchanged = app
            .state
            .apply_service_liveness(vec![ServiceLivenessProbeResult {
                workspace_id,
                service_id: 1,
                liveness: ServiceLiveness::Up,
            }]);
        assert!(!unchanged);
    }
}
