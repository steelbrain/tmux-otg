# tmux-otg

Read-only HTTP server that lists and live-tails allowlisted tmux sessions over Server-Sent Events.

## What it is

`tmux-otg` ("tmux on the go") is a tiny HTTP server, built with [axum](https://github.com/tokio-rs/axum) and [tokio](https://tokio.rs/), that lets you watch the output of selected tmux sessions from a browser. It renders a minimal UI listing your allowlisted sessions and, for each one, live-tails the active pane by pushing the captured pane text over Server-Sent Events (SSE) on a fixed interval.

It is built for the case where you want to glance at a long-running tmux session (a build, a server log, a training run) from another device on your localhost or [Tailscale](https://tailscale.com/) tailnet, without installing anything on the client. Multiple viewers of the same session share a single capture loop, so the number of `tmux` processes scales with the number of distinct sessions being watched, not with the number of open browsers.

### What it does NOT do

`tmux-otg` is strictly a **viewer**. It does not — and has no endpoint that could —

- send keystrokes or input to a session,
- control, attach to, resize, create, rename, or kill a session,
- run arbitrary shell commands,
- mutate any tmux or system state in any way.

The only tmux operations it can ever perform are `tmux list-sessions` and `tmux capture-pane`, both of which are read-only.

## Security model

Please read this section before exposing the server to anything other than your own machine.

### The `public-insecure-` allowlist

A tmux session is **only** ever listed or streamed if its name begins with the prefix:

```
public-insecure-
```

This prefix is hardcoded and **not configurable** — the entire security model rests on it. A session is invisible to `tmux-otg` unless its owner deliberately opted in by naming it `public-insecure-…`. Sessions named anything else cannot be enumerated, viewed, or streamed.

The prefix literally contains the word **insecure** as a reminder: anything you put in such a session may be read by anyone who can reach the server.

### Strict character allowlist for names

On top of the prefix check, a session name is accepted only if it:

- begins with `public-insecure-`,
- has **at least one** character after the prefix (a bare `public-insecure-` is rejected),
- is no longer than **128 bytes**, and
- consists solely of ASCII alphanumerics, `-`, and `_`.

This guarantees a name can never contain shell metacharacters (`;`, `|`, `&`, `$()`, backticks, …), whitespace, control characters, path traversal (`../`), or the `:` / `.` characters tmux uses to address windows and panes. These rules live in `src/validation.rs` and are covered by unit tests.

### No shell interpolation

Every call into tmux is made through `std::process::Command` with explicit, separate arguments — the server never constructs a shell command string. A session name is always passed as the *value* of `-t`, so it can never be reinterpreted as a flag or a command. The capture path re-validates the name against the allowlist as defense in depth, even though the HTTP layer has already checked it.

### Read-only endpoint surface

There is no input, control, or mutation endpoint anywhere in the server. See [HTTP endpoints](#http-endpoints) for the complete list of routes.

### Binding and the threat model

- By default the server binds to **`127.0.0.1`** (localhost), so it is reachable only from the local machine.
- **There is no authentication.** None. Anything the server is bound to is readable by anyone who can reach that address.
- If you set `--host` to a non-localhost address (for example, your Tailscale IP), then **anyone who can reach that address can read every `public-insecure-*` session**, with no login or token. The server prints a loud warning at startup whenever it binds to a non-loopback address.

Treat the contents of any `public-insecure-*` session as public to everyone on the network you bind to. Only name a session `public-insecure-*` if you are comfortable with those people seeing its output.

As defense in depth, every response also carries a restrictive `Content-Security-Policy` (no external resources, no framing) along with `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.

## Requirements

- A Rust toolchain supporting **edition 2024** (recent stable Rust; see `Cargo.toml`).
- A working **`tmux`** on your `PATH`. The server runs `tmux -V` at startup and exits immediately with a clear error if tmux cannot be executed.

## Build & run

Build a release binary:

```sh
cargo build --release
```

The binary is produced at:

```
target/release/tmux-otg
```

Run it:

```sh
./target/release/tmux-otg
```

Or build-and-run in one step during development:

```sh
cargo run
```

On startup the server prints the address it is listening on and the active allowlist prefix, for example:

```
tmux-otg: listening on http://127.0.0.1:8080  (read-only; allowlist: public-insecure-*)
```

## Configuration

All options can be set either via a CLI flag or its environment-variable equivalent. CLI flags take precedence over environment variables.

| Flag | Env var | Default | Accepted values |
|------|---------|---------|-----------------|
| `--host <IP>` | `TMUX_OTG_HOST` | `127.0.0.1` | any IP address |
| `--port <PORT>` | `TMUX_OTG_PORT` | `8080` | any valid TCP port (`0`–`65535`) |
| `--interval-secs <N>` | `TMUX_OTG_INTERVAL` | `2` | `1`–`3600` (seconds between pane refreshes) |
| `--scrollback <N>` | `TMUX_OTG_SCROLLBACK` | `0` | `0`–`100000` (extra history lines above the visible pane; `0` = visible pane only) |

Example:

```sh
TMUX_OTG_PORT=9000 ./target/release/tmux-otg --interval-secs 5 --scrollback 500
```

## HTTP endpoints

| Route | Description |
|-------|-------------|
| `GET /` | Index page listing the allowlisted (`public-insecure-*`) sessions currently running. |
| `GET /view/{name}` | Session view page — a small HTML shell that subscribes to the session's SSE stream. Returns 404 if the name is not allowlisted. |
| `GET /stream/{name}` | Server-Sent Events stream that pushes the latest captured pane text on the configured interval. Returns 404 if the name is not allowlisted. |
| `GET /healthz` | Liveness check; returns `ok`. |

Any other path returns a 404 page.

## Quick start

1. Create (or rename) a tmux session with an allowlisted name:

   ```sh
   tmux new -s public-insecure-demo
   ```

2. In another terminal, start the server:

   ```sh
   cargo run
   ```

3. Open the UI in your browser:

   ```
   http://127.0.0.1:8080
   ```

   You should see `public-insecure-demo` in the list. Click it to live-tail its active pane.

### Exposing over Tailscale (optional)

To make the viewer reachable from other devices on your tailnet, bind to your Tailscale IP:

```sh
./target/release/tmux-otg --host 100.x.y.z
```

then open `http://100.x.y.z:8080` from any tailnet device.

**Caveat:** this is unauthenticated. Everyone who can reach that address can read every `public-insecure-*` session. See the [Security model](#security-model) above.

## Testing

```sh
cargo test
```

There are currently 23 tests covering configuration parsing, the name allowlist and validation rules, the tmux command wrappers, the per-session capturer/fan-out hub, HTML rendering/escaping, and the HTTP handlers (allowlist gate, health, fallback, and security headers).

## License

MIT.
