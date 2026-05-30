//! Runtime configuration, sourced from CLI flags with environment-variable
//! fallbacks.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use clap::Parser;

/// Read-only HTTP server that tails allowlisted tmux sessions over SSE.
///
/// Only sessions named `public-insecure-*` are ever listed or streamed. There
/// are no control, input, or shell endpoints. Built for localhost and Tailscale.
#[derive(Debug, Clone, Parser)]
#[command(name = "tmux-otg", version, about, long_about = None)]
pub struct Config {
    /// Host/IP address to bind. Defaults to localhost; set this to your
    /// Tailscale IP to expose the server over your tailnet.
    #[arg(long, env = "TMUX_OTG_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// TCP port to listen on.
    #[arg(long, env = "TMUX_OTG_PORT", default_value_t = 8080)]
    pub port: u16,

    /// Seconds between pane refreshes pushed over SSE.
    #[arg(
        long,
        env = "TMUX_OTG_INTERVAL",
        default_value_t = 2,
        value_parser = clap::value_parser!(u64).range(1..=3600),
    )]
    pub interval_secs: u64,

    /// Extra scrollback history lines to include above the visible pane
    /// (0 = visible pane only).
    #[arg(
        long,
        env = "TMUX_OTG_SCROLLBACK",
        default_value_t = 0,
        value_parser = clap::value_parser!(u32).range(0..=100_000),
    )]
    pub scrollback: u32,
}

impl Config {
    /// The socket address the server should bind to.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    /// The refresh interval as a [`Duration`].
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::net::Ipv4Addr;

    #[test]
    fn defaults_are_localhost_8080() {
        let cfg = Config::parse_from(["tmux-otg"]);
        assert_eq!(cfg.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.interval_secs, 2);
        assert_eq!(cfg.scrollback, 0);
        assert_eq!(
            cfg.socket_addr(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 8080))
        );
    }

    #[test]
    fn flags_override_defaults() {
        let cfg = Config::parse_from([
            "tmux-otg",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--interval-secs",
            "5",
            "--scrollback",
            "500",
        ]);
        assert_eq!(cfg.host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.interval(), Duration::from_secs(5));
        assert_eq!(cfg.scrollback, 500);
    }

    #[test]
    fn rejects_interval_below_minimum() {
        assert!(Config::try_parse_from(["tmux-otg", "--interval-secs", "0"]).is_err());
    }
}
