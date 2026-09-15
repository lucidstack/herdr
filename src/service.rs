//! Long-running dev server/listening port registry.

use std::borrow::Cow;

/// A user-registered long-running dev server/listening endpoint owned by a workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Service {
    /// Workspace-scoped monotonic ID; never reused after removal.
    pub id: u64,
    pub label: String,
    pub url: String,
    /// Free-text attribution of the registering source (e.g. "cli", "plugin:<id>").
    pub source: String,
    /// Server-computed; never trusted across a restart. Not serialized.
    #[serde(skip)]
    pub liveness: ServiceLiveness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceLiveness {
    #[default]
    Unknown,
    Up,
    Down,
}

impl Service {
    /// Creates a new service with liveness initialised to Unknown.
    pub fn new(
        id: u64,
        label: impl Into<String>,
        url: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id,
            label: label.into(),
            url: url.into(),
            source: source.into(),
            liveness: ServiceLiveness::Unknown,
        }
    }

    /// Normalises a URL to http/https URL or resolvable host:port form.
    /// Accepts: "http://...", "https://...", "host:port", "localhost:3000", ":3000" (→ http://localhost:3000).
    /// Returns None for invalid URLs (missing host or non-numeric port).
    pub fn normalise_url(input: &str) -> Option<Cow<'static, str>> {
        let trimmed = input.trim();

        // Already a valid http/https URL.
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            // Basic validation: must have some content after protocol.
            if trimmed.len() > 8 && !trimmed.ends_with("://") {
                return Some(Cow::Owned(trimmed.to_string()));
            }
            return None;
        }

        // Bare host:port or :port format.
        if let Some(colon_idx) = trimmed.rfind(':') {
            let host_part = &trimmed[..colon_idx];
            let port_part = &trimmed[colon_idx + 1..];

            // Validate port is numeric and in valid range.
            if let Ok(_port) = port_part.parse::<u16>() {
                let host = if host_part.is_empty() {
                    "localhost"
                } else {
                    host_part
                };
                return Some(Cow::Owned(format!("http://{}:{}", host, port_part)));
            }
            return None;
        }

        None
    }
}

/// Removes a service by ID or label (one must be Some).
pub fn remove_service_by_id_or_label(
    services: &mut Vec<Service>,
    id: Option<u64>,
    label: Option<&str>,
) -> Option<Service> {
    match (id, label) {
        (Some(id), _) => services
            .iter()
            .position(|s| s.id == id)
            .map(|pos| services.remove(pos)),
        (None, Some(label)) => services
            .iter()
            .position(|s| s.label == label)
            .map(|pos| services.remove(pos)),
        _ => None,
    }
}

/// Finds a service by label; useful for UPSERT logic.
pub fn find_service_by_label(services: &[Service], label: &str) -> Option<usize> {
    services.iter().position(|s| s.label == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_new_initialises_liveness_to_unknown() {
        let svc = Service::new(1, "Rails", "http://localhost:3000", "cli");
        assert_eq!(svc.id, 1);
        assert_eq!(svc.label, "Rails");
        assert_eq!(svc.liveness, ServiceLiveness::Unknown);
    }

    #[test]
    fn normalise_url_accepts_http_https() {
        assert_eq!(
            Service::normalise_url("http://localhost:3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            Service::normalise_url("https://api.example.com").as_deref(),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn normalise_url_accepts_host_port() {
        assert_eq!(
            Service::normalise_url("localhost:3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            Service::normalise_url("example.com:8080").as_deref(),
            Some("http://example.com:8080")
        );
    }

    #[test]
    fn normalise_url_normalises_bare_port() {
        assert_eq!(
            Service::normalise_url(":3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            Service::normalise_url(":8000").as_deref(),
            Some("http://localhost:8000")
        );
    }

    #[test]
    fn normalise_url_rejects_invalid_port() {
        assert_eq!(Service::normalise_url("localhost:abc"), None);
        assert_eq!(Service::normalise_url(":99999"), None);
        assert_eq!(Service::normalise_url(""), None);
    }

    #[test]
    fn remove_service_by_id() {
        let mut services = vec![
            Service::new(1, "A", "http://a", "cli"),
            Service::new(2, "B", "http://b", "cli"),
        ];
        let removed = remove_service_by_id_or_label(&mut services, Some(1), None);
        assert_eq!(removed.map(|s| s.label), Some("A".into()));
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].label, "B");
    }

    #[test]
    fn remove_service_by_label() {
        let mut services = vec![
            Service::new(1, "Rails", "http://a", "cli"),
            Service::new(2, "Webpack", "http://b", "cli"),
        ];
        let removed = remove_service_by_id_or_label(&mut services, None, Some("Rails"));
        assert_eq!(removed.map(|s| s.id), Some(1));
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].label, "Webpack");
    }

    #[test]
    fn test_find_service_by_label() {
        let services = vec![
            Service::new(1, "A", "http://a", "cli"),
            Service::new(2, "B", "http://b", "cli"),
        ];
        assert_eq!(find_service_by_label(&services, "B"), Some(1));
        assert_eq!(find_service_by_label(&services, "C"), None);
    }
}
