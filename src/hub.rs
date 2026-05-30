//! Per-session pane capturing with subscriber fan-out.
//!
//! The first client to view a session starts a single background "capturer"
//! task for it. Every additional client subscribes to that same task instead
//! of spawning its own `tmux capture-pane` loop, so the number of concurrent
//! `tmux` processes is bounded by the number of *distinct* sessions being
//! watched, not by the number of connected clients. When the last subscriber
//! for a session disconnects, its capturer stops and the registry entry is
//! removed.
//!
//! The shared channel is a [`tokio::sync::watch`]: subscribers only ever care
//! about the *latest* pane snapshot, so slow or bursty clients simply observe
//! the most recent value rather than a backlog.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::tmux;

/// The latest known state of a session's pane, broadcast to all subscribers.
#[derive(Clone)]
pub enum PaneUpdate {
    /// No capture has completed yet; subscribers wait without emitting.
    Pending,
    /// A JSON-encoded pane snapshot, ready to place in an SSE `pane` event.
    /// Encoded once per capture and shared (`Arc`) across all subscribers.
    Pane(Arc<str>),
    /// Terminal state: a JSON-encoded reason, ready for an SSE `gone` event.
    /// No further updates follow.
    Gone(Arc<str>),
}

/// A registered capturer: the channel that feeds its subscribers, tagged with a
/// unique `epoch` so a capturer only ever removes *its own* registry entry (see
/// [`Hub::stop_if_idle`]).
struct Session {
    epoch: u64,
    tx: watch::Sender<PaneUpdate>,
}

/// Shared registry of active per-session capturers.
pub struct Hub {
    sessions: Mutex<HashMap<String, Session>>,
    next_epoch: AtomicU64,
    interval: Duration,
    scrollback: u32,
}

impl Hub {
    /// Creates an empty hub. `interval` and `scrollback` are applied to every
    /// capturer it spawns.
    pub fn new(interval: Duration, scrollback: u32) -> Self {
        Hub {
            sessions: Mutex::new(HashMap::new()),
            next_epoch: AtomicU64::new(0),
            interval,
            scrollback,
        }
    }

    /// Subscribes to live updates for `session`, starting a capturer if this is
    /// the first subscriber. Callers must have already validated `session`
    /// against the allowlist (see [`crate::validation::is_allowed_session`]).
    pub fn subscribe(self: &Arc<Self>, session: &str) -> watch::Receiver<PaneUpdate> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(existing) = sessions.get(session) {
            // A capturer is already running; just attach to it.
            return existing.tx.subscribe();
        }
        // First subscriber: create the channel, register it, and spawn the
        // single capturer task that will feed every subscriber.
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        let (tx, _seed) = watch::channel(PaneUpdate::Pending);
        let rx = tx.subscribe();
        sessions.insert(
            session.to_string(),
            Session {
                epoch,
                tx: tx.clone(),
            },
        );
        tokio::spawn(Arc::clone(self).run_capturer(session.to_string(), tx, epoch));
        rx
    }

    /// The capture loop for a single session. Runs until the session goes away
    /// (or fails) or until no subscribers remain. `epoch` identifies this
    /// capturer's registry entry so it never disturbs a successor's.
    async fn run_capturer(
        self: Arc<Self>,
        session: String,
        tx: watch::Sender<PaneUpdate>,
        epoch: u64,
    ) {
        loop {
            let target = session.clone();
            let scrollback = self.scrollback;
            let captured =
                tokio::task::spawn_blocking(move || tmux::capture_pane(&target, scrollback)).await;

            match captured {
                Ok(Ok(text)) => {
                    tx.send_replace(PaneUpdate::Pane(encode(&text)));
                }
                Ok(Err(err)) => {
                    // The session likely went away (or tmux failed). Log the
                    // detail server-side, tell subscribers generically, drop the
                    // registry entry, and stop. The `send_replace` MUST precede
                    // `remove`/`return`: it advances the watch version while the
                    // channel is still open, so subscribers observe `Gone`
                    // rather than just a closed channel.
                    eprintln!("tmux-otg: capture failed for {session}: {err}");
                    tx.send_replace(PaneUpdate::Gone(encode("session unavailable")));
                    self.remove(&session, epoch);
                    return;
                }
                Err(join) => {
                    // The blocking capture task panicked or was cancelled.
                    // Surface it to subscribers rather than ending silently
                    // (otherwise EventSource would just reconnect into the same
                    // failure forever). Same ordering rule as above.
                    eprintln!("tmux-otg: capture task for {session} failed: {join}");
                    tx.send_replace(PaneUpdate::Gone(encode("session unavailable")));
                    self.remove(&session, epoch);
                    return;
                }
            }

            // Sleep *after* capturing so that a capture slower than `interval`
            // can never compound into a tight back-to-back loop.
            tokio::time::sleep(self.interval).await;

            // Stop once nobody is listening. The receiver-count check and the
            // registry removal happen under the same lock `subscribe` takes, so
            // a subscriber arriving concurrently is never stranded on a dead
            // capturer (see `stop_if_idle`).
            if self.stop_if_idle(&session, &tx, epoch) {
                return;
            }
        }
    }

    /// Removes this capturer's entry for `session` on a terminal state, but only
    /// if the registered entry is still ours (matching `epoch`). The epoch guard
    /// keeps the "one capturer per session" invariant locally checkable: it can
    /// never delete a successor capturer's freshly inserted entry.
    fn remove(&self, session: &str, epoch: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(session).is_some_and(|s| s.epoch == epoch) {
            sessions.remove(session);
        }
    }

    /// Removes this capturer's entry and returns `true` iff no subscribers
    /// remain.
    ///
    /// Holding the lock across the `receiver_count` check closes the race with
    /// [`Hub::subscribe`]: either a newly arriving subscriber has already been
    /// counted (we keep running), or it has not yet inserted — in which case it
    /// will find the entry gone and create a fresh capturer after we remove. The
    /// `epoch` guard ensures we only ever remove our own entry.
    fn stop_if_idle(&self, session: &str, tx: &watch::Sender<PaneUpdate>, epoch: u64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if tx.receiver_count() == 0 {
            if sessions.get(session).is_some_and(|s| s.epoch == epoch) {
                sessions.remove(session);
            }
            true
        } else {
            false
        }
    }

    /// Test/diagnostic helper: number of sessions with a live capturer.
    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// JSON-encodes `text` into a single line so embedded newlines cannot break SSE
/// framing; the client `JSON.parse`s it back into the original multi-line text.
fn encode(text: &str) -> Arc<str> {
    serde_json::to_string(text)
        .unwrap_or_else(|_| String::from("\"\""))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_produces_a_parseable_json_string() {
        let got = encode("line one\nline two");
        // Must be a single JSON string literal (no raw newline) so SSE framing
        // is preserved; round-trips back to the original text.
        assert!(!got.contains('\n'));
        let back: String = serde_json::from_str(&got).unwrap();
        assert_eq!(back, "line one\nline two");
    }

    #[tokio::test]
    async fn second_subscriber_shares_one_capturer() {
        // Uses a session name that is allowlisted but (almost certainly) does
        // not exist, so the capturer's first `capture-pane` fails and it ends.
        // Before that happens, two subscribers must share a single entry.
        let hub = Arc::new(Hub::new(Duration::from_secs(60), 0));
        let _rx1 = hub.subscribe("public-insecure-shared-capturer-test");
        let _rx2 = hub.subscribe("public-insecure-shared-capturer-test");
        assert_eq!(hub.active_count(), 1, "both subscribers share one capturer");
    }
}
