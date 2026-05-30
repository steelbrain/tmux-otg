//! HTTP layer: routing, request handlers, the SSE stream, and the (tiny) HTML
//! UI. Nothing here can mutate a session — the only tmux operations reachable
//! are `list-sessions` and `capture-pane`.

use std::convert::Infallible;
use std::sync::Arc;

use async_stream::stream;
use axum::{
    Router,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};

use crate::hub::{Hub, PaneUpdate};
use crate::tmux;
use crate::validation;

/// Shared hub handed to every request; owns the per-session capturers.
pub type SharedHub = Arc<Hub>;

/// Restrictive Content-Security-Policy for the tiny self-contained UI: no
/// external resources at all, inline `<style>`/`<script>` only, same-origin
/// connections (for the SSE stream), and no framing (anti-clickjacking).
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// Builds the application router with all routes wired to `hub`.
pub fn router(hub: SharedHub) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/view/{name}", get(view))
        .route("/stream/{name}", get(stream))
        .route("/healthz", get(health))
        .fallback(fallback)
        .layer(middleware::from_fn(security_headers))
        .with_state(hub)
}

/// Attaches defense-in-depth security headers to every response.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res
}

async fn health() -> &'static str {
    "ok\n"
}

async fn fallback() -> Response {
    not_found()
}

/// Index page: lists the allowlisted sessions currently running.
async fn index() -> Response {
    match list_sessions().await {
        Ok(sessions) => Html(render_index(&sessions)).into_response(),
        Err(err) => {
            eprintln!("tmux-otg: error listing sessions: {err}");
            internal_error("could not list tmux sessions")
        }
    }
}

/// Session view page: a thin shell that subscribes to the SSE stream.
async fn view(Path(name): Path<String>) -> Response {
    if !validation::is_allowed_session(&name) {
        return not_found();
    }
    Html(render_view(&name)).into_response()
}

/// SSE endpoint: subscribes the client to the session's shared capturer and
/// relays each new pane snapshot. Many clients on the same session all share a
/// single capturer (and thus a single `tmux capture-pane` loop).
async fn stream(State(hub): State<SharedHub>, Path(name): Path<String>) -> Response {
    if !validation::is_allowed_session(&name) {
        return not_found();
    }
    let mut rx = hub.subscribe(&name);

    let body = stream! {
        loop {
            // Clone the current snapshot out so the watch borrow is not held
            // across an await point. Cloning is cheap (an `Arc` bump). A freshly
            // subscribed receiver sees the latest value immediately, so a late
            // joiner paints at once instead of waiting for the next capture.
            let update = rx.borrow_and_update().clone();
            match update {
                PaneUpdate::Pending => {} // nothing captured yet; keep waiting
                PaneUpdate::Pane(payload) => {
                    yield Ok::<Event, Infallible>(Event::default().event("pane").data(payload));
                }
                PaneUpdate::Gone(payload) => {
                    yield Ok::<Event, Infallible>(Event::default().event("gone").data(payload));
                    break;
                }
            }

            if rx.changed().await.is_err() {
                // All senders dropped without a terminal `Gone` (e.g. the
                // capturer task was cancelled). Tell the client and stop.
                let payload = serde_json::to_string("session unavailable")
                    .unwrap_or_else(|_| String::from("\"\""));
                yield Ok::<Event, Infallible>(Event::default().event("gone").data(payload));
                break;
            }
        }
    };

    Sse::new(body).keep_alive(KeepAlive::default()).into_response()
}

/// Runs the blocking `list_allowed_sessions` on the blocking pool.
async fn list_sessions() -> Result<Vec<String>, tmux::TmuxError> {
    match tokio::task::spawn_blocking(tmux::list_allowed_sessions).await {
        Ok(res) => res,
        Err(join) => Err(tmux::TmuxError::Task(join.to_string())),
    }
}

// --- Responses -------------------------------------------------------------

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(render_error(
            "404 — not found",
            "No such session, or it is not in the public-insecure-* allowlist.",
        )),
    )
        .into_response()
}

fn internal_error(detail: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Html(render_error("500 — error", detail))).into_response()
}

// --- Templates -------------------------------------------------------------

const STYLE: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; padding: 1.5rem; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #0d1117; color: #c9d1d9; line-height: 1.5; }
a { color: #58a6ff; text-decoration: none; }
a:hover { text-decoration: underline; }
h1 { font-size: 1.2rem; margin: 0; }
.note { color: #8b949e; font-size: 0.85rem; margin: 0.25rem 0 1.25rem; }
.empty { color: #8b949e; }
ul { list-style: none; padding: 0; margin: 0; }
li { margin: 0.4rem 0; }
li a { display: inline-block; padding: 0.5rem 0.75rem; background: #161b22; border: 1px solid #30363d; border-radius: 6px; }
.topbar { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap; margin-bottom: 0.75rem; }
#status { font-size: 0.8rem; color: #8b949e; }
pre { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 1rem; overflow: auto; white-space: pre-wrap; word-break: break-word; max-height: 80vh; margin: 0; }
"#;

/// Minimal HTML page wrapper. `body` is assumed to be already-safe markup;
/// `title` is escaped.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>{STYLE}</style>\n\
         </head>\n\
         <body>\n{body}\n</body>\n</html>\n",
        title = escape_html(title),
    )
}

fn render_index(sessions: &[String]) -> String {
    let list = if sessions.is_empty() {
        "<p class=\"empty\">No <code>public-insecure-*</code> sessions are currently running.</p>"
            .to_string()
    } else {
        let items: String = sessions
            .iter()
            .map(|name| {
                let safe = escape_html(name);
                format!("<li><a href=\"/view/{safe}\">{safe}</a></li>")
            })
            .collect();
        format!("<ul>{items}</ul>")
    };
    let body = format!(
        "<h1>tmux-otg</h1>\n\
         <p class=\"note\">Read-only. Listing tmux sessions named <code>public-insecure-*</code>.</p>\n\
         {list}",
    );
    page("tmux-otg — sessions", &body)
}

fn render_view(name: &str) -> String {
    let safe = escape_html(name);
    // Encode the URL as a JSON string literal so it is always a valid, safely
    // quoted JS string (the name is already restricted to a safe charset).
    let stream_url =
        serde_json::to_string(&format!("/stream/{name}")).unwrap_or_else(|_| "\"\"".to_string());
    let body = format!(
        r#"<div class="topbar">
  <h1>{safe}</h1>
  <a href="/">← all sessions</a>
  <span id="status">connecting…</span>
</div>
<pre id="out"></pre>
<script>
const out = document.getElementById('out');
const status = document.getElementById('status');
const url = {stream_url};
const es = new EventSource(url);
es.addEventListener('pane', (e) => {{
  out.textContent = JSON.parse(e.data);
  status.textContent = 'live';
  out.scrollTop = out.scrollHeight;
}});
es.addEventListener('gone', (e) => {{
  status.textContent = JSON.parse(e.data);
  es.close();
}});
es.onerror = () => {{ status.textContent = 'disconnected — retrying…'; }};
</script>"#,
    );
    page(&format!("tmux-otg — {name}"), &body)
}

fn render_error(heading: &str, detail: &str) -> String {
    let body = format!(
        "<h1>{}</h1>\n\
         <p class=\"note\">{}</p>\n\
         <p><a href=\"/\">← back to sessions</a></p>",
        escape_html(heading),
        escape_html(detail),
    );
    page(heading, &body)
}

/// Escapes the five characters that matter for HTML text/attribute contexts.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_neutralizes_markup() {
        assert_eq!(escape_html("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#x27;");
        // allowlisted session characters are passed through untouched
        assert_eq!(escape_html("public-insecure-demo_1"), "public-insecure-demo_1");
    }

    #[test]
    fn index_renders_links_and_empty_state() {
        let html = render_index(&["public-insecure-a".to_string()]);
        assert!(html.contains("href=\"/view/public-insecure-a\""));

        let empty = render_index(&[]);
        assert!(empty.contains("No <code>public-insecure-*</code> sessions"));
        assert!(!empty.contains("<ul>"));
    }

    #[test]
    fn view_embeds_the_stream_url_as_json() {
        let html = render_view("public-insecure-demo");
        assert!(html.contains(r#"const url = "/stream/public-insecure-demo""#));
        assert!(html.contains("new EventSource(url)"));
    }

    // --- Handler-level integration tests -----------------------------------
    //
    // These drive the router via `oneshot` to lock in the security-relevant
    // behaviour at the HTTP edge: the allowlist gate on `/view` and `/stream`,
    // the health/fallback routes, and the security headers. None of these
    // requests reach tmux: `/view` only renders HTML, and the rejected
    // `/stream` request returns before any capturer is started.
    mod handlers {
        use super::super::*;
        use axum::body::Body;
        use axum::http::{Request, StatusCode, header};
        use std::time::Duration;
        use tower::ServiceExt;

        fn app() -> Router {
            router(Arc::new(Hub::new(Duration::from_secs(60), 0)))
        }

        async fn get(uri: &str) -> Response {
            app().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap()
        }

        #[tokio::test]
        async fn healthz_returns_ok() {
            let res = get("/healthz").await;
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&bytes[..], b"ok\n");
        }

        #[tokio::test]
        async fn view_rejects_non_allowlisted_name() {
            assert_eq!(get("/view/private-session").await.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn view_accepts_allowlisted_name() {
            assert_eq!(get("/view/public-insecure-demo").await.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn stream_rejects_non_allowlisted_name() {
            // The allowlist gate runs before any capturer is started, so this
            // is safe to assert without tmux.
            assert_eq!(get("/stream/not-allowed").await.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn unknown_route_is_not_found() {
            assert_eq!(get("/no/such/path").await.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn responses_carry_security_headers() {
            let res = get("/healthz").await;
            let headers = res.headers();
            assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY), "CSP header present");
            assert_eq!(headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
            assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
        }
    }
}
