use std::collections::HashMap;
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::events::AppEvent;
use crate::service::ServiceLiveness;
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct ServiceLivenessProbeResult {
    pub workspace_id: String,
    pub service_id: u64,
    pub liveness: ServiceLiveness,
}

pub struct ServiceLivenessProber {
    last_probe_completed: Option<Instant>,
    probe_interval: Duration,
    event_tx: mpsc::Sender<AppEvent>,
}

impl ServiceLivenessProber {
    pub fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            last_probe_completed: None,
            probe_interval: Duration::from_secs(5),
            event_tx,
        }
    }

    pub fn maybe_probe(&mut self, workspaces: &[Workspace]) {
        // Only probe if at least one service exists
        let has_services = workspaces.iter().any(|ws| !ws.services.is_empty());
        if !has_services {
            self.last_probe_completed = None;
            return;
        }

        // Only schedule probe if last one completed and enough time has passed
        if let Some(last_completed) = self.last_probe_completed {
            if last_completed.elapsed() < self.probe_interval {
                return;
            }
        }

        // Mark as in-flight immediately to prevent double-scheduling
        self.last_probe_completed = Some(Instant::now());

        // Collect services to probe
        let mut services_to_probe: Vec<(String, u64, String)> = Vec::new();
        for ws in workspaces {
            for svc in &ws.services {
                services_to_probe.push((ws.id.clone(), svc.id, svc.url.clone()));
            }
        }

        if services_to_probe.is_empty() {
            return;
        }

        let event_tx = self.event_tx.clone();

        // Spawn background probe thread
        std::thread::spawn(move || {
            let mut results = Vec::new();

            for (workspace_id, service_id, url) in services_to_probe {
                let liveness = probe_service_liveness(&url);
                results.push(ServiceLivenessProbeResult {
                    workspace_id,
                    service_id,
                    liveness,
                });
            }

            // Send results back to main loop
            let _ = event_tx.blocking_send(AppEvent::ServiceLivenessProbed { results });
        });
    }
}

fn probe_service_liveness(url: &str) -> ServiceLiveness {
    // Parse URL to get host:port
    let host_port = match parse_url_host_port(url) {
        Some(hp) => hp,
        None => return ServiceLiveness::Down,
    };

    // Try to resolve and connect with 500ms timeout
    match std::net::ToSocketAddrs::to_socket_addrs(&host_port) {
        Ok(addrs) => {
            // Take first resolved address
            if let Some(addr) = addrs.take(1).next() {
                match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                    Ok(_) => ServiceLiveness::Up,
                    Err(_) => ServiceLiveness::Down,
                }
            } else {
                ServiceLiveness::Down
            }
        }
        Err(_) => ServiceLiveness::Down,
    }
}

fn parse_url_host_port(url: &str) -> Option<String> {
    // Simple parser for http://host:port or https://host:port
    let url = url.trim();
    let url = if url.starts_with("https://") {
        &url[8..]
    } else if url.starts_with("http://") {
        &url[7..]
    } else {
        url
    };

    // Remove path if present
    let url = if let Some(idx) = url.find('/') {
        &url[..idx]
    } else {
        url
    };

    // Check if it already has a port
    if url.contains(':') {
        Some(url.to_string())
    } else {
        // Infer port from scheme (we don't have scheme here, so default to 80)
        Some(format!("{url}:80"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_host_port_with_explicit_port() {
        assert_eq!(
            parse_url_host_port("http://localhost:3000"),
            Some("localhost:3000".into())
        );
    }

    #[test]
    fn parse_url_host_port_without_port() {
        assert_eq!(
            parse_url_host_port("http://localhost"),
            Some("localhost:80".into())
        );
    }

    #[test]
    fn parse_url_host_port_with_https() {
        assert_eq!(
            parse_url_host_port("https://example.com:8443"),
            Some("example.com:8443".into())
        );
    }

    #[test]
    fn parse_url_host_port_with_path() {
        assert_eq!(
            parse_url_host_port("http://localhost:3000/api"),
            Some("localhost:3000".into())
        );
    }
}
