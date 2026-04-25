//! `mlink dev` — start the daemon, open the in-browser debug page, and wait
//! for ctrl-c. We split `mlink_daemon::run()` into `bind_and_prepare` +
//! `await_shutdown` so we can learn the bound port before blocking on the
//! serve future; otherwise the browser would race the listener.

use crate::util::open_url::open_url;

pub(crate) async fn cmd_dev() -> Result<(), mlink_daemon::DaemonError> {
    let (port, serve) = mlink_daemon::bind_and_prepare().await?;
    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("[mlink-dev] opening {url}");
    if let Err(e) = open_url(&url) {
        // Not fatal — the daemon is live and the URL is printed above, so a
        // manual click still works.
        eprintln!("[mlink-dev] could not auto-open browser: {e}");
        eprintln!("[mlink-dev] open {url} manually");
    }
    serve.await_shutdown().await
}
