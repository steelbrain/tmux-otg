//! tmux-otg — a small, read-only HTTP server that tails allowlisted tmux
//! sessions over Server-Sent Events.
//!
//! Security model in one breath: it binds to localhost by default, only ever
//! exposes sessions named `public-insecure-*`, offers no input/control/shell
//! endpoints, and only ever runs `tmux list-sessions` / `tmux capture-pane`
//! with explicit (never shell-interpolated) arguments.

mod config;
mod http;
mod hub;
mod tmux;
mod validation;

use std::process::Command;
use std::sync::Arc;

use clap::Parser;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("tmux-otg: fatal: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::parse();
    let addr = config.socket_addr();

    // Fail fast with a clear message if tmux isn't usable, rather than only
    // surfacing it on the first request.
    if let Err(err) = Command::new("tmux").arg("-V").output() {
        return Err(
            format!("could not execute `tmux` (is it installed and on PATH?): {err}").into()
        );
    }

    let hub = Arc::new(hub::Hub::new(config.interval(), config.scrollback));
    let app = http::router(hub);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;

    eprintln!(
        "tmux-otg: listening on http://{addr}  (read-only; allowlist: {}*)",
        validation::ALLOWED_PREFIX
    );

    // The whole security model assumes a trusted network. Binding anywhere but
    // loopback (e.g. a Tailscale IP) publishes every exposed pane to anyone who
    // can reach the address, with no authentication — make that loud.
    if !addr.ip().is_loopback() {
        eprintln!(
            "tmux-otg: WARNING: bound to non-loopback address {addr} with NO authentication — \
             every {}* pane is readable by anyone who can reach it.",
            validation::ALLOWED_PREFIX
        );
    }

    axum::serve(listener, app).await?;
    Ok(())
}
