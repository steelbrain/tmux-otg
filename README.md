# tmux-otg

Watch your tmux sessions live from a browser — strictly read-only, over Server-Sent Events.

`tmux-otg` ("tmux on the go") is a tiny HTTP server, built on [axum](https://github.com/tokio-rs/axum) and [tokio](https://tokio.rs/), that live-tails selected tmux sessions to a browser. It serves a minimal UI listing your allowlisted sessions and streams each one's active pane over Server-Sent Events (SSE) on a fixed interval — handy for glancing at a long-running build, server log, or training run from another device on your localhost or [Tailscale](https://tailscale.com/) tailnet, with nothing to install on the client. Multiple viewers of one session share a single capture loop, so the number of `tmux` processes scales with the distinct sessions being watched, not the number of open browsers. It is **strictly a viewer** — the only tmux operations it can ever perform are `list-sessions` and `capture-pane` — and it only ever exposes sessions named `public-insecure-*`. It binds to localhost with no authentication; **read the [Security model](#security-model) before exposing it anywhere else.**

## Setup

### Requirements

- A working **`tmux`** on your `PATH`. The server runs `tmux -V` at startup and exits immediately with a clear error if tmux cannot be executed.
- A Rust toolchain supporting **edition 2024** (recent stable; see `Cargo.toml`) — only needed when building from source.

### From a release

Prebuilt binaries for macOS and Linux (`x86_64` and `aarch64`) are attached to every [GitHub Release](https://github.com/steelbrain/tmux-otg/releases):

```sh
tar -xzf tmux-otg-<platform>.tar.gz
./tmux-otg-<platform>/tmux-otg
```

### From source

```sh
cargo build --release
```

The binary is produced at `target/release/tmux-otg`. During development, `cargo run` builds and runs in one step.

### Running

```sh
./target/release/tmux-otg
```

On startup the server prints the address it is listening on and the active allowlist prefix:

```
tmux-otg: listening on http://127.0.0.1:8080  (read-only; allowlist: public-insecure-*)
```

## Quick start

1. Create (or rename) a tmux session with an allowlisted name:

   ```sh
   tmux new -s public-insecure-demo
   ```

2. In another terminal, start the server:

   ```sh
   cargo run
   ```

3. Open the UI at `http://127.0.0.1:8080`. You should see `public-insecure-demo` in the list — click it to live-tail its active pane.

### Exposing over Tailscale (optional)

To reach the viewer from other devices on your tailnet, bind to your Tailscale IP:

```sh
./target/release/tmux-otg --host 100.x.y.z
```

then open `http://100.x.y.z:8080` from any tailnet device. **This is unauthenticated** — everyone who can reach that address can read every `public-insecure-*` session. See the [Security model](#security-model).

## HTTP endpoints

| Route | Description |
|-------|-------------|
| `GET /` | Index page listing the allowlisted (`public-insecure-*`) sessions currently running. If no tmux server is running yet, this returns the normal empty state rather than a 500. |
| `GET /view/{name}` | Session view page — a small HTML shell that subscribes to the session's SSE stream. Returns 404 if the name is not allowlisted. |
| `GET /stream/{name}` | Server-Sent Events stream that pushes the latest captured pane text on the configured interval. Returns 404 if the name is not allowlisted. |
| `GET /healthz` | Liveness check; returns `ok`. |

Any other path returns a 404 page. There is no input, control, or mutation endpoint anywhere in the server.

## Configuration

All options can be set via a CLI flag or its environment-variable equivalent. CLI flags take precedence over environment variables.

| Flag | Env var | Default | Accepted values |
|------|---------|---------|-----------------|
| `--host <IP>` | `TMUX_OTG_HOST` | `127.0.0.1` | any IP address (e.g. `0.0.0.0` to listen on all interfaces) |
| `--port <PORT>` | `TMUX_OTG_PORT` | `8080` | any valid TCP port (`0`–`65535`) |
| `--interval-secs <N>` | `TMUX_OTG_INTERVAL` | `2` | `1`–`3600` (seconds between pane refreshes) |
| `--scrollback <N>` | `TMUX_OTG_SCROLLBACK` | `0` | `0`–`100000` (extra history lines above the visible pane; `0` = visible pane only) |

Example:

```sh
TMUX_OTG_PORT=9000 ./target/release/tmux-otg --interval-secs 5 --scrollback 500
```

## Security model

Please read this section before exposing the server to anything other than your own machine.

### The `public-insecure-` allowlist

A tmux session is **only** ever listed or streamed if its name begins with the prefix:

```
public-insecure-
```

This prefix is hardcoded and **not configurable** — the entire security model rests on it. A session is invisible to `tmux-otg` unless its owner deliberately opted in by naming it `public-insecure-…`; sessions named anything else cannot be enumerated, viewed, or streamed. The prefix literally contains the word **insecure** as a reminder: anything you put in such a session may be read by anyone who can reach the server.

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

`tmux-otg` is strictly a **viewer**. It does not — and has no endpoint that could —

- send keystrokes or input to a session,
- control, attach to, resize, create, rename, or kill a session,
- run arbitrary shell commands,
- mutate any tmux or system state in any way.

The only tmux operations it can ever perform are `tmux list-sessions` and `tmux capture-pane`, both read-only.

### Binding and the threat model

- By default the server binds to **`127.0.0.1`** (localhost), so it is reachable only from the local machine.
- **There is no authentication.** None. Anything the server is bound to is readable by anyone who can reach that address.
- If you set `--host` to a non-localhost address — `0.0.0.0` to listen on **all** network interfaces, or a specific address such as your Tailscale IP — then **anyone who can reach that address can read every `public-insecure-*` session**, with no login or token. The server prints a loud warning at startup whenever it binds to a non-loopback address.

Treat the contents of any `public-insecure-*` session as public to everyone on the network you bind to. As defense in depth, every response also carries a restrictive `Content-Security-Policy` (no external resources, no framing) along with `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.

## Development

```sh
cargo run                                              # build and run locally (debug)
cargo fmt --all                                        # auto-format
cargo fmt --all --check && cargo clippy --all-targets  # lint (formatting + clippy)
cargo test                                             # run the test suite
cargo build --release                                  # optimized build
```

The suite currently has 34 tests covering configuration parsing, the name allowlist and validation rules, the tmux command wrappers, the per-session capturer/fan-out hub, HTML rendering/escaping, and the HTTP handlers (allowlist gate, health, fallback, security headers, empty-state handling when no tmux server is running, and the tail view's pinned-only auto-scroll behavior). CI runs formatting, clippy (`-D warnings`), tests, and a release build on every push and pull request.

## License

MIT
