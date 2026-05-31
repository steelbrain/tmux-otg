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
/// external resources at all, inline `<style>`/`<script>` only, the favicon and
/// other images from our own origin (`img-src 'self'`), same-origin connections
/// (for the SSE stream), and no framing (anti-clickjacking).
const CSP: &str = "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; \
     script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// The favicon and iOS home-screen icon, embedded at compile time so the server
/// stays a single self-contained binary with no asset files to ship. All are
/// served same-origin (hence `img-src 'self'` in [`CSP`]). The favicon is an SVG
/// (crisp at any size) with a 32x32 PNG fallback for browsers that don't support
/// SVG favicons.
const FAVICON_SVG: &[u8] = include_bytes!("../assets/favicon.svg");
const FAVICON_PNG: &[u8] = include_bytes!("../assets/favicon.png");
const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../assets/apple-touch-icon.png");

/// Builds the application router with all routes wired to `hub`.
pub fn router(hub: SharedHub) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/view/{name}", get(view))
        .route("/stream/{name}", get(stream))
        .route("/healthz", get(health))
        .route("/favicon.svg", get(favicon_svg))
        .route("/favicon.png", get(favicon_png))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
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

/// Serves the browser-tab favicon as SVG — the primary icon, crisp at any size.
async fn favicon_svg() -> Response {
    static_asset(FAVICON_SVG, "image/svg+xml")
}

/// Serves the PNG favicon — a fallback for browsers without SVG-favicon support.
async fn favicon_png() -> Response {
    static_asset(FAVICON_PNG, "image/png")
}

/// Serves the iOS "add to home screen" icon — handy since the whole point is
/// glancing at sessions from a phone.
async fn apple_touch_icon() -> Response {
    static_asset(APPLE_TOUCH_ICON_PNG, "image/png")
}

/// Builds a response for an embedded static asset, overriding the default
/// `application/octet-stream` content type and adding a day-long cache hint so
/// browsers don't refetch the icon on every page load.
fn static_asset(bytes: &'static [u8], content_type: &'static str) -> Response {
    let mut res = bytes.into_response();
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    res
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
         <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\n\
         <link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\">\n\
         <link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">\n\
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
// Each refresh replaces the whole pane snapshot. Only follow the tail when the
// viewer is already at (or near) the bottom; if they have scrolled up to read
// scrollback, keep their position instead of snapping them back down. The
// slack absorbs sub-pixel rounding and lets "almost at the bottom" still pin.
const PIN_SLACK_PX = 24;
function atBottom() {{
  return out.scrollHeight - out.scrollTop - out.clientHeight <= PIN_SLACK_PX;
}}
const es = new EventSource(url);
es.addEventListener('pane', (e) => {{
  const pinned = atBottom();
  const prevTop = out.scrollTop;
  out.textContent = JSON.parse(e.data);
  status.textContent = 'live';
  if (pinned) {{
    out.scrollTop = out.scrollHeight;
  }} else {{
    out.scrollTop = prevTop;
  }}
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
    fn pages_link_the_favicon_and_touch_icon() {
        // Every page goes through `page()`, so checking one is enough to pin the
        // icon links into the shared <head>.
        let html = render_index(&[]);
        assert!(html.contains(r#"<link rel="icon" type="image/svg+xml" href="/favicon.svg">"#));
        assert!(html.contains(r#"<link rel="icon" type="image/png" href="/favicon.png">"#));
        assert!(html.contains(r#"<link rel="apple-touch-icon" href="/apple-touch-icon.png">"#));
    }

    #[test]
    fn view_embeds_the_stream_url_as_json() {
        let html = render_view("public-insecure-demo");
        assert!(html.contains(r#"const url = "/stream/public-insecure-demo""#));
        assert!(html.contains("new EventSource(url)"));
    }

    #[test]
    fn view_only_autoscrolls_when_pinned_to_bottom() {
        let html = render_view("public-insecure-demo");
        // The tail view must not snap to the bottom on every refresh; it only
        // follows new output when the viewer is already near the bottom, and
        // otherwise restores their scroll position so reading scrollback (e.g.
        // on a phone) is not interrupted every refresh.
        assert!(html.contains("function atBottom()"));
        assert!(html.contains("const pinned = atBottom();"));
        assert!(html.contains("out.scrollTop = prevTop;"));
        // It must still follow the tail when the viewer is pinned to the bottom.
        assert!(html.contains("out.scrollTop = out.scrollHeight;"));
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
            // Pin the exact CSP value, not just its presence: invariant #8 in
            // AGENTS.md depends on these specific directives (no external
            // resources, same-origin SSE only, no framing). A regression that
            // widened `default-src` or dropped `frame-ancestors 'none'` would
            // slip past a presence-only check.
            assert_eq!(headers.get(header::CONTENT_SECURITY_POLICY).unwrap(), CSP);
            assert!(CSP.contains("default-src 'none'"));
            assert!(CSP.contains("connect-src 'self'"));
            assert!(CSP.contains("frame-ancestors 'none'"));
            // The favicon is served from our own origin, so images are confined
            // to `'self'` — never widened to external hosts or `data:`.
            assert!(CSP.contains("img-src 'self'"));
            assert_eq!(headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
            assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
        }

        #[tokio::test]
        async fn edge_rejects_prefixed_names_with_target_or_injection_chars() {
            // A name can carry the allowlisted prefix yet still smuggle a tmux
            // target separator (`:` / `.`). Both /view and /stream must reject
            // these at the HTTP edge — before any capturer or tmux process —
            // so the security model does not rest on validation.rs alone.
            for path in [
                "/view/public-insecure-a:1",
                "/view/public-insecure-a.0",
                "/stream/public-insecure-a:1",
                "/stream/public-insecure-a.0",
            ] {
                assert_eq!(
                    get(path).await.status(),
                    StatusCode::NOT_FOUND,
                    "{path} must be rejected at the edge",
                );
            }
        }

        #[tokio::test]
        async fn favicon_svg_is_served_as_svg() {
            let res = get("/favicon.svg").await;
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "image/svg+xml");
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            let body = std::str::from_utf8(&bytes).unwrap();
            // Real SVG markup, not octet-stream junk.
            assert!(body.contains("<svg"));
        }

        #[tokio::test]
        async fn favicon_is_served_as_a_png() {
            let res = get("/favicon.png").await;
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "image/png");
            // A cache hint so browsers don't refetch the icon on every page load.
            assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), "public, max-age=86400");
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            // PNG magic number — proves we served real image bytes and that the
            // content type was overridden from the default octet-stream.
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        }

        #[tokio::test]
        async fn apple_touch_icon_is_served_as_a_png() {
            let res = get("/apple-touch-icon.png").await;
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "image/png");
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        }
    }
}
