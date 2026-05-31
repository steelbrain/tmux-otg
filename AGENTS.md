# AGENTS.md

Guidance for AI coding agents working in this repository. Everything below is verified against the source — keep it accurate as the code evolves.

## Project overview

`tmux-otg` ("tmux on the go") is a small, read-only HTTP server that lists and live-tails tmux sessions over Server-Sent Events (SSE). It is built on axum 0.8 + tokio (Rust edition 2024). It binds to localhost (`127.0.0.1:8080`) by default and is intended to optionally be exposed over a Tailscale tailnet. Its entire reason for being safe rests on three things: it only ever exposes sessions named `public-insecure-*`, it offers no input/control/shell endpoint, and it only ever invokes `tmux list-sessions` / `tmux capture-pane` with explicit, non-shell-interpolated arguments.

## Architecture / module map

Standard single-binary Cargo layout; `src/main.rs` is the entry point and declares the five other modules.

- **`src/main.rs`** — Binary entry point. Parses `Config`, fails fast if `tmux -V` cannot be executed, constructs the `Hub`, builds the router, binds the `TcpListener`, prints a loud stderr warning if bound to a non-loopback address (no auth), and serves via `axum::serve`. Declares the `config`, `http`, `hub`, `tmux`, and `validation` modules.
- **`src/config.rs`** — Runtime configuration via `clap` derive, sourced from CLI flags with environment-variable fallbacks (`TMUX_OTG_HOST`, `TMUX_OTG_PORT`, `TMUX_OTG_INTERVAL`, `TMUX_OTG_SCROLLBACK`). Defaults: host `127.0.0.1`, port `8080`, interval `2s` (range `1..=3600`), scrollback `0` (range `0..=100_000`). Exposes `socket_addr()` and `interval()`.
- **`src/validation.rs`** — Pure (no I/O), security-critical allowlist logic. Defines `ALLOWED_PREFIX = "public-insecure-"`, `MAX_NAME_LEN = 128`, `is_allowed_session()`, and `filter_allowed()`. This is the heart of the security model and is heavily unit-tested.
- **`src/tmux.rs`** — Thin synchronous wrappers around the `tmux` CLI using `std::process::Command` with separate args (never a shell string). Exposes `list_allowed_sessions()` (returns empty list when no server is running) and `capture_pane()` (re-validates the name as defense in depth). Defines `TmuxError`.
- **`src/hub.rs`** — Per-session pane capturing with subscriber fan-out. The first viewer of a session starts a single background capturer task; every additional viewer subscribes to that same task over a `tokio::sync::watch` channel, so the number of concurrent `tmux capture-pane` processes is bounded by the number of *distinct* sessions watched, not by the number of clients. The capturer is ref-counted (via `watch::Sender::receiver_count`) and removes itself when the last subscriber leaves; an `epoch` tag on each registry entry keeps the one-capturer-per-session invariant locally checkable. Defines `Hub` and the `PaneUpdate` broadcast enum.
- **`src/http.rs`** — HTTP layer: router, handlers, the SSE relay, a security-headers middleware (`Content-Security-Policy`, `X-Content-Type-Options: nosniff`, `Referrer-Policy`), and the tiny inline HTML/JS UI. Routes: `GET /`, `GET /view/{name}`, `GET /stream/{name}`, `GET /healthz`, plus a 404 fallback. The `/stream` handler subscribes to the `Hub` and relays each `PaneUpdate`; the actual capture loop lives in `hub.rs`. Contains `escape_html()` and the HTML/SSE templating.

## Build / test / lint commands

Standard Cargo workflow:

```sh
cargo build              # debug build
cargo build --release    # optimized build (release profile strips symbols)
cargo test               # run unit + handler tests (config, validation, tmux, hub, http)
cargo clippy             # lint (run with --all-targets to include tests)
```

## Security model and its invariants

This is the most important section. The server is only safe because of the guarantees below. **Any change that touches these must preserve them and ship with tests.** If you cannot keep an invariant, stop and flag it rather than weakening it.

1. **Hardcoded allowlist prefix.** Only sessions whose names begin with `ALLOWED_PREFIX` (`"public-insecure-"`, defined in `src/validation.rs`) may ever be listed, viewed, or streamed. This is intentionally **not** configurable — do not add a flag or env var to override it. There must also be at least one character after the prefix (the bare prefix is rejected).
2. **Strict character allowlist + length cap.** A session name is valid only if it consists solely of `[A-Za-z0-9_-]` (ASCII alphanumerics, `-`, `_`) and is no longer than `MAX_NAME_LEN` (128 bytes). This guarantees a name can never contain shell metacharacters, whitespace, control characters, or the `:`/`.` characters tmux uses to address windows and panes. Do not widen this charset.
3. **No shell interpolation, ever.** All tmux invocations use `std::process::Command` with explicit, separate arguments. Never build a shell string, never pass a user value through `sh -c`, and always pass session names as the *value* of `-t` (so they cannot be parsed as flags). See `src/tmux.rs`.
4. **Read-only contract.** The only tmux operations reachable are `list-sessions` and `capture-pane`. There are no input, control, send-keys, attach, kill, or shell endpoints — and there must never be. Do not add any route or tmux call that can mutate a session or run arbitrary commands.
5. **Never kill tmux sessions as part of agent work.** This applies especially to review agents, subagents, and diagnostic helpers. Do not run `tmux kill-server`, do not bulk-kill sessions, and do not kill any session you did not create unless the user explicitly asks. This repo exists to observe long-lived tmux sessions, and killing them can destroy active work and pane history. When investigating tmux behavior, prefer non-destructive commands (`list-sessions`, `capture-pane`, `has-session`) over any lifecycle-changing command.
6. **Defense in depth.** `capture_pane()` re-validates the name even though HTTP handlers already check it. Keep redundant validation at the boundary closest to `tmux`.
7. **Output escaping.** All session names rendered into HTML go through `escape_html()` (escapes `& < > " '`). Pane text and the stream URL are JSON-encoded (`serde_json`) so embedded newlines/quotes can't break SSE framing or the inline `<script>`. Preserve this encoding for any new output paths.
8. **Defense-in-depth headers.** Every response carries a restrictive `Content-Security-Policy` (no external resources; inline `<style>`/`<script>` and same-origin SSE only; `frame-ancestors 'none'`), plus `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. If you add external assets you will need to widen the CSP — do so as narrowly as possible.
9. **Loud non-loopback warning.** Binding anywhere but loopback (there is no authentication) prints a stderr warning at startup. Keep that warning if you touch the bind path.

When in doubt: prefer rejecting input over accepting it, and add a unit test that pins the behavior.

## Conventions

**Testing.** Any new Rust behavior — or any change to existing behavior — must ship with tests in the same change. Add or update the unit/handler tests that pin the behavior so it cannot silently regress; this applies to all code, not only the security-critical logic called out above. If something genuinely cannot be tested, note why in the commit rather than skip coverage silently.

**Branching.** This project commits directly to `main` — do not create feature sub-branches or open PRs unless explicitly asked.

**Commit messages.** Present-tense imperative mood ("Add", not "Added"/"Adds"), first line ≤ 72 characters, with an emoji prefix:

| Emoji | When to use |
|-------|-------------|
| `:new:` | Adding new functionality |
| `:bug:` | Fixing a bug |
| `:fire:` | Removing code or files |
| `:memo:` | Writing docs |
| `:white_check_mark:` | Adding tests |
| `:art:` | Improving format/structure |
| `:arrow_up:` | Upgrading a dependency |
| `:lock:` | Security |

Documentation-only commits MUST include `[ci skip]` in the title. If no emoji fits, omit it rather than force-fitting one.

**Pull requests.** PR titles omit the emoji prefix (emojis are for commit messages only).

## Documentation and working style

Every source file already carries module-level doc comments (`//!`) that describe its responsibility and, where relevant, the security rationale. Keep these accurate whenever you change behavior. Prefer small, independently reviewable changes that come with tests rather than large end-to-end rewrites — the security-critical logic in `validation.rs` and `tmux.rs` especially should never change without accompanying test coverage.

Keep release-facing docs focused on non-obvious behavior, operator-facing usage, and security-relevant details. Do not add README churn for behavior that is already the natural/expected outcome of the documented interface unless it changes user decisions, operating steps, or the threat model.
