//! HTTP routing surface for the daemon. Hosts the axum `Router` plus the two
//! leaf handlers — `GET /` (inlined web-debug page) and `GET /ws` (upgrade
//! into a session). Split out of `lib.rs` so the crate root only deals with
//! process lifecycle + shared state.

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::session;
use crate::DaemonState;

/// Inlined web-debug page. Served at `GET /` so `mlink dev` can open a browser
/// straight at the daemon with no external file dependency — handy for ad-hoc
/// debugging without checking out the repo. Kept as a raw `include_str!` so
/// any change to the HTML is picked up on the next build.
pub const WEB_DEBUG_HTML: &str = include_str!("../../../examples/web-debug/index.html");

/// Build the axum router. Split out so tests can mount the same routes on an
/// arbitrary listener without going through `run()`.
pub fn router(state: DaemonState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn index_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        WEB_DEBUG_HTML,
    )
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session::run(socket, state))
}
