//! Thin, synchronous wrappers around the `tmux` CLI.
//!
//! Every call uses [`std::process::Command`] with explicit, separate arguments
//! — we never build a shell string — so a session name can never be
//! interpreted as part of a command or as additional flags. Callers must still
//! pass only names that have passed [`crate::validation::is_allowed_session`];
//! [`capture_pane`] re-checks this as defense in depth.

use std::process::Command;

use crate::validation;

/// Errors that can arise while talking to tmux.
#[derive(Debug)]
pub enum TmuxError {
    /// The `tmux` binary could not be executed at all (e.g. not installed).
    Spawn(std::io::Error),
    /// tmux ran but reported failure. `stderr` is lossily decoded UTF-8.
    Command { code: Option<i32>, stderr: String },
    /// A caller passed a name that is not allowlisted — a programmer error.
    Rejected(String),
    /// A background task running a tmux command panicked or was cancelled
    /// before it could report a result. tmux itself may never have been run.
    Task(String),
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::Spawn(e) => write!(f, "failed to run tmux: {e}"),
            TmuxError::Command { code, stderr } => {
                let status = match code {
                    Some(c) => c.to_string(),
                    None => "signal".to_string(),
                };
                write!(f, "tmux exited with status {status}: {}", stderr.trim())
            }
            TmuxError::Rejected(name) => write!(f, "session name not allowed: {name}"),
            TmuxError::Task(msg) => write!(f, "tmux task failed: {msg}"),
        }
    }
}

impl std::error::Error for TmuxError {}

/// Recognizes the benign "there is no server to talk to / nothing to list"
/// stderr from `tmux list-sessions`, as opposed to a real failure.
///
/// tmux phrases this several ways depending on platform and version:
/// * `no server running on <socket>` — the server was never started (common
///   on Linux and when tmux has its own default socket).
/// * `error connecting to <socket> (No such file or directory)` — the socket
///   file is absent (common on macOS), or `(Connection refused)` for a stale
///   socket left behind by a server that has since exited.
/// * `no sessions` — the server is up but empty (older tmux).
///
/// For this read-only viewer every one of these is the normal "nothing to
/// show yet" state, so `list_allowed_sessions` maps them to an empty list
/// rather than surfacing an HTTP 500. The match is case-insensitive so it does
/// not hinge on tmux's exact capitalization.
fn is_idle_server_stderr(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no server running")
        || s.contains("no sessions")
        || s.contains("error connecting to")
}

/// Lists the names of all tmux sessions that are allowed to be exposed.
///
/// If no tmux server is running, this returns an empty list rather than an
/// error — that is the normal "nothing to show yet" state.
pub fn list_allowed_sessions() -> Result<Vec<String>, TmuxError> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(TmuxError::Spawn)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // No server / no sessions is the normal idle state, not an error for us.
        if is_idle_server_stderr(&stderr) {
            return Ok(Vec::new());
        }
        return Err(TmuxError::Command { code: output.status.code(), stderr: stderr.into_owned() });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(validation::filter_allowed(stdout.lines()))
}

/// Captures the current contents of the active pane of `session`.
///
/// `scrollback` requests that many extra history lines above the visible pane
/// (0 = visible pane only). The returned string preserves newlines. Wrapped
/// lines are rejoined (`-J`) for readability.
pub fn capture_pane(session: &str, scrollback: u32) -> Result<String, TmuxError> {
    // Belt-and-suspenders: never shell out for a name that isn't allowlisted,
    // even though callers are expected to have already checked.
    if !validation::is_allowed_session(session) {
        return Err(TmuxError::Rejected(session.to_string()));
    }

    let mut cmd = Command::new("tmux");
    cmd.args(["capture-pane", "-p", "-J"]);
    if scrollback > 0 {
        // `-S -<n>` starts the capture n lines back in the history.
        cmd.arg("-S");
        cmd.arg(format!("-{scrollback}"));
    }
    // The session name is the *value* of `-t`, so it is never parsed as a flag.
    cmd.arg("-t");
    cmd.arg(session);

    let output = cmd.output().map_err(TmuxError::Spawn)?;
    if !output.status.success() {
        return Err(TmuxError::Command {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_rejects_non_allowlisted_names_without_spawning() {
        // These must fail fast on validation, never reaching tmux.
        let err = capture_pane("private-session", 0).unwrap_err();
        assert!(matches!(err, TmuxError::Rejected(_)));

        let err = capture_pane("public-insecure-a;rm -rf /", 0).unwrap_err();
        assert!(matches!(err, TmuxError::Rejected(_)));
    }

    #[test]
    fn idle_stderr_covers_the_no_server_family() {
        // macOS / tmux with a missing socket (verified against tmux 3.6b):
        // this message contains neither "no server running" nor "no sessions",
        // so it must still be recognized as the idle state, not an HTTP 500.
        assert!(is_idle_server_stderr(
            "error connecting to /private/tmp/tmux-501/default (No such file or directory)"
        ));
        // Linux / default-socket phrasing.
        assert!(is_idle_server_stderr("no server running on /tmp/tmux-1000/default"));
        // Stale socket left by a dead server.
        assert!(is_idle_server_stderr(
            "error connecting to /tmp/tmux-1000/default (Connection refused)"
        ));
        // Server up but empty (older tmux).
        assert!(is_idle_server_stderr("no sessions"));
        // Capitalization must not matter.
        assert!(is_idle_server_stderr("No Server Running on /tmp/x"));
    }

    #[test]
    fn idle_stderr_does_not_swallow_real_errors() {
        assert!(!is_idle_server_stderr("server exited unexpectedly"));
        assert!(!is_idle_server_stderr("out of memory"));
        assert!(!is_idle_server_stderr("protocol version mismatch"));
        assert!(!is_idle_server_stderr(""));
    }

    #[test]
    fn error_display_is_readable() {
        let e = TmuxError::Command { code: Some(1), stderr: "  can't find session  ".to_string() };
        assert_eq!(e.to_string(), "tmux exited with status 1: can't find session");

        let e = TmuxError::Rejected("nope".to_string());
        assert_eq!(e.to_string(), "session name not allowed: nope");
    }

    #[test]
    fn error_display_covers_signal_and_task() {
        // A signal-terminated tmux has no exit code and renders as "signal".
        let e = TmuxError::Command { code: None, stderr: "  killed  ".to_string() };
        assert_eq!(e.to_string(), "tmux exited with status signal: killed");

        let e = TmuxError::Task("join panicked".to_string());
        assert_eq!(e.to_string(), "tmux task failed: join panicked");
    }
}
