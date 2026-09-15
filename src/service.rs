//! Long-running dev servers and listening ports registered against a workspace.

/// A registered long-running dev server or listening endpoint owned by a workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Service {
    /// Workspace-scoped monotonic id; never reused after removal.
    pub id: u64,
    pub label: String,
    /// Normalised `http://` or `https://` URL; see [`normalise_url`].
    pub url: String,
    /// Free-text attribution of the registering source (for example `cli` or `plugin:<id>`).
    pub source: String,
    /// Server-computed by the liveness probe; never trusted across a restart.
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

    /// The `(host, port)` the liveness probe connects to, derived from the
    /// normalised URL. Missing ports default from the scheme.
    pub fn probe_target(&self) -> Option<(String, u16)> {
        probe_target(&self.url)
    }
}

/// Normalises user input into an `http://` or `https://` URL with a resolvable host.
///
/// Accepted forms: `http://host[:port][/path]`, `https://host[:port][/path]`,
/// `host:port`, and `:port` (which becomes `http://localhost:port`). Anything
/// without a host, or with a non-numeric port, is rejected.
pub fn normalise_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return probe_target(trimmed).map(|_| trimmed.to_string());
    }
    if trimmed.contains("://") || trimmed.contains('/') {
        return None;
    }
    let (host, port) = trimmed.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    let host = if host.is_empty() { "localhost" } else { host };
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return None;
    }
    Some(format!("http://{host}:{port}"))
}

fn probe_target(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        return None;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse::<u16>().ok()?,
            None if tail.is_empty() => default_port,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), port.parse::<u16>().ok()?))
        }
        Some(_) => None,
        None => Some((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_url_keeps_full_urls() {
        assert_eq!(
            normalise_url(" http://localhost:3000/admin ").as_deref(),
            Some("http://localhost:3000/admin")
        );
        assert_eq!(
            normalise_url("https://api.example.com").as_deref(),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn normalise_url_expands_host_port_and_bare_port() {
        assert_eq!(
            normalise_url("example.com:8080").as_deref(),
            Some("http://example.com:8080")
        );
        assert_eq!(
            normalise_url(":3000").as_deref(),
            Some("http://localhost:3000")
        );
    }

    #[test]
    fn normalise_url_rejects_unresolvable_input() {
        for input in [
            "",
            "localhost",
            "localhost:abc",
            ":99999",
            "http://",
            "http://:3000",
            "ftp://host:21",
            "rails server",
        ] {
            assert_eq!(normalise_url(input), None, "input {input:?}");
        }
    }

    #[test]
    fn probe_target_defaults_port_from_scheme() {
        assert_eq!(
            probe_target("http://localhost"),
            Some(("localhost".into(), 80))
        );
        assert_eq!(
            probe_target("https://example.com/path?x=1"),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            probe_target("https://user@example.com:8443"),
            Some(("example.com".into(), 8443))
        );
    }

    #[test]
    fn probe_target_handles_ipv6_literals() {
        assert_eq!(
            probe_target("http://[::1]:3000"),
            Some(("::1".into(), 3000))
        );
        assert_eq!(probe_target("http://[::1]"), Some(("::1".into(), 80)));
        assert_eq!(probe_target("http://::1:3000"), None);
    }
}
