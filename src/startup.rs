//! Connection hints; the entry point separately prints the configured API Key.
use pi_agent::config::Config;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

pub fn print(config: &Config, bound: SocketAddr, server_id: Option<&str>) {
    eprintln!("{}", summary(config, bound, server_id, route_ip(bound)));
}

fn summary(
    config: &Config,
    bound: SocketAddr,
    server_id: Option<&str>,
    route: Option<IpAddr>,
) -> String {
    let local = if bound.ip().is_unspecified() {
        if bound.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        }
    } else {
        bound.ip()
    };
    let mut lines = vec![
        format!("  Server ID: {}", server_id.unwrap_or("not configured")),
        format!(
            "  MCP endpoint: {}",
            endpoint(SocketAddr::new(local, bound.port()))
        ),
        format!("  Authentication: {}", authentication(config)),
        format!(
            "  Features: projects={}, harness={}, sync={}",
            config.projects.is_some(),
            !config.harnesses.is_empty(),
            config.sync.as_ref().is_some_and(|s| s.enabled)
        ),
    ];
    if bound.ip().is_unspecified() {
        lines.push(
            match route.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()) {
                Some(ip) => format!(
                    "  Remote MCP candidate: {} (verify client reachability)",
                    endpoint(SocketAddr::new(ip, bound.port()))
                ),
                None => format!("  Remote MCP: http://<server-ip>:{}/mcp", bound.port()),
            },
        );
    }
    lines.join("\n")
}

fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}/mcp")
}

fn authentication(config: &Config) -> String {
    if config.password.is_some() || config.password_env.is_some() {
        let username = config
            .username
            .clone()
            .or_else(|| std::env::var("FS_SERVER_USER").ok())
            .unwrap_or_default();
        let source = config
            .password_env
            .as_deref()
            .map(|name| format!("environment variable {name}"))
            .unwrap_or_else(|| "config field password".into());
        format!("Basic; username={username}; password from {source}")
    } else {
        let source = config
            .token_env
            .as_deref()
            .map(|name| format!("environment variable {name}"))
            .unwrap_or_else(|| "config field api_key (legacy: token)".into());
        format!("Bearer (MCP API Key); token from {source}")
    }
}

fn route_ip(bound: SocketAddr) -> Option<IpAddr> {
    if !bound.ip().is_unspecified() {
        return None;
    }
    // UDP connect selects a source address without sending a datagram.
    let (local, destination) = if bound.is_ipv4() {
        ("0.0.0.0:0", "192.0.2.1:9")
    } else {
        ("[::]:0", "[2001:db8::1]:9")
    };
    let socket = UdpSocket::bind(local).ok()?;
    socket.connect(destination).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(auth: &str) -> Config {
        toml::from_str(&format!("listen='0.0.0.0:8787'\n{auth}")).unwrap()
    }

    #[test]
    fn wildcard_bind_shows_connectable_candidates_without_disclosing_password() {
        let config = config("username='li'\npassword='private-password-value'");
        let text = summary(
            &config,
            "0.0.0.0:8787".parse().unwrap(),
            Some("node"),
            Some("192.168.1.12".parse().unwrap()),
        );
        assert!(text.contains("http://127.0.0.1:8787/mcp"));
        assert!(text.contains("http://192.168.1.12:8787/mcp"));
        assert!(text.contains("Basic; username=li; password from config field password"));
        assert!(!text.contains("private-password-value"));
        assert!(!text.contains("http://0.0.0.0"));
    }

    #[test]
    fn specific_ipv6_bind_preserves_address_and_does_not_advertise_other_interfaces() {
        let config = config("token='private-token-value-at-least-24-bytes'");
        let text = summary(
            &config,
            "[::1]:8787".parse().unwrap(),
            None,
            Some("192.168.1.12".parse().unwrap()),
        );
        assert!(text.contains("http://[::1]:8787/mcp"));
        assert!(text.contains("Bearer (MCP API Key); token from config field api_key"));
        assert!(!text.contains("private-token-value"));
        assert!(!text.contains("Remote MCP"));
    }

    #[test]
    fn environment_authentication_reports_only_the_reference() {
        let config = config("token_env='PI_AGENT_TOKEN'");
        let text = summary(&config, "0.0.0.0:43210".parse().unwrap(), None, None);
        assert!(text.contains("token from environment variable PI_AGENT_TOKEN"));
        assert!(text.contains("<server-ip>"));
        assert!(text.contains(":43210"));
    }
}
