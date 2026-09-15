//! Background liveness probe for registered workspace services.
//!
//! Modelled on the Git status refresh: the headless poll loop asks the prober
//! whether a probe is due, the probe itself runs on a plain OS thread, and the
//! results come back through the app event channel. Nothing here is reachable
//! from view computation or rendering.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::events::AppEvent;
use crate::service::ServiceLiveness;
use crate::workspace::Workspace;

pub const SERVICE_LIVENESS_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const SERVICE_LIVENESS_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLivenessProbeResult {
    pub workspace_id: String,
    pub service_id: u64,
    pub liveness: ServiceLiveness,
}

struct ProbeTarget {
    workspace_id: String,
    service_id: u64,
    address: Option<(String, u16)>,
}

pub struct ServiceLivenessProber {
    in_flight: bool,
    last_probe_completed: Option<Instant>,
    event_tx: mpsc::Sender<AppEvent>,
}

impl ServiceLivenessProber {
    pub fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            in_flight: false,
            last_probe_completed: None,
            event_tx,
        }
    }

    /// Whether a probe should start now. False while one is in flight, when no
    /// workspace has services, or until the interval since the last completed
    /// probe has elapsed.
    pub fn is_due(&self, now: Instant, workspaces: &[Workspace]) -> bool {
        if self.in_flight || workspaces.iter().all(|ws| ws.services.is_empty()) {
            return false;
        }
        self.last_probe_completed.is_none_or(|last| {
            now.saturating_duration_since(last) >= SERVICE_LIVENESS_PROBE_INTERVAL
        })
    }

    /// Starts a background probe when due. Returns whether one was started.
    pub fn start_if_due(&mut self, now: Instant, workspaces: &[Workspace]) -> bool {
        if !self.is_due(now, workspaces) {
            return false;
        }
        let targets: Vec<ProbeTarget> = workspaces
            .iter()
            .flat_map(|ws| {
                ws.services.iter().map(|service| ProbeTarget {
                    workspace_id: ws.id.clone(),
                    service_id: service.id,
                    address: service.probe_target(),
                })
            })
            .collect();
        let event_tx = self.event_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("herdr-service-liveness".into())
            .spawn(move || {
                let results = targets
                    .into_iter()
                    .map(|target| ServiceLivenessProbeResult {
                        workspace_id: target.workspace_id,
                        service_id: target.service_id,
                        liveness: target
                            .address
                            .map_or(ServiceLiveness::Down, |(host, port)| {
                                probe_address(&host, port)
                            }),
                    })
                    .collect();
                let _ = event_tx.blocking_send(AppEvent::ServiceLivenessProbed { results });
            });
        match spawned {
            Ok(_) => {
                self.in_flight = true;
                true
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to spawn service liveness probe thread");
                false
            }
        }
    }

    /// Records that the in-flight probe delivered its results.
    pub fn mark_probe_completed(&mut self, now: Instant) {
        self.in_flight = false;
        self.last_probe_completed = Some(now);
    }
}

fn probe_address(host: &str, port: u16) -> ServiceLiveness {
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return ServiceLiveness::Down;
    };
    let Some(addr) = addrs.next() else {
        return ServiceLiveness::Down;
    };
    match TcpStream::connect_timeout(&addr, SERVICE_LIVENESS_CONNECT_TIMEOUT) {
        Ok(_) => ServiceLiveness::Up,
        Err(_) => ServiceLiveness::Down,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Service;

    fn prober() -> ServiceLivenessProber {
        let (event_tx, _event_rx) = mpsc::channel(8);
        ServiceLivenessProber::new(event_tx)
    }

    fn workspace_with_services(count: usize) -> Workspace {
        let mut workspace = Workspace::test_new("svc");
        for id in 1..=count as u64 {
            workspace
                .services
                .push(Service::new(id, format!("svc-{id}"), ":3000", "test"));
        }
        workspace
    }

    #[test]
    fn probe_is_not_due_without_services() {
        let prober = prober();
        assert!(!prober.is_due(Instant::now(), &[]));
        assert!(!prober.is_due(Instant::now(), &[Workspace::test_new("svc")]));
    }

    #[test]
    fn first_probe_is_due_immediately_once_a_service_exists() {
        let prober = prober();
        assert!(prober.is_due(Instant::now(), &[workspace_with_services(1)]));
    }

    #[test]
    fn probe_is_not_rescheduled_while_in_flight() {
        let mut prober = prober();
        let workspaces = [workspace_with_services(1)];
        let now = Instant::now();
        assert!(prober.start_if_due(now, &workspaces));
        assert!(!prober.is_due(now + SERVICE_LIVENESS_PROBE_INTERVAL * 3, &workspaces));
    }

    #[test]
    fn completed_probe_waits_for_the_interval_before_rescheduling() {
        let mut prober = prober();
        let workspaces = [workspace_with_services(1)];
        let started = Instant::now();
        assert!(prober.start_if_due(started, &workspaces));
        let completed = started + Duration::from_secs(1);
        prober.mark_probe_completed(completed);
        assert!(!prober.is_due(completed + Duration::from_secs(1), &workspaces));
        assert!(prober.is_due(completed + SERVICE_LIVENESS_PROBE_INTERVAL, &workspaces));
    }

    #[test]
    fn unresolvable_target_reports_down() {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut prober = ServiceLivenessProber::new(event_tx);
        let mut workspace = Workspace::test_new("svc");
        workspace.services.push(Service::new(
            7,
            "broken",
            "http://nonexistent.invalid.:1",
            "test",
        ));
        let workspace_id = workspace.id.clone();
        assert!(prober.start_if_due(Instant::now(), &[workspace]));
        let Some(AppEvent::ServiceLivenessProbed { results }) = event_rx.blocking_recv() else {
            panic!("expected a probe result event");
        };
        assert_eq!(
            results,
            vec![ServiceLivenessProbeResult {
                workspace_id,
                service_id: 7,
                liveness: ServiceLiveness::Down,
            }]
        );
    }
}
