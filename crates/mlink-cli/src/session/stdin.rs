//! Line-oriented stdin reader used by interactive chat mode. The active
//! variant spawns a tokio task and streams non-empty lines through an mpsc;
//! the inert variant drops the sender so `recv()` returns `None` forever
//! (cmd_join without `--chat` uses this so the main select! arm stays a
//! no-op without any special casing).

use tokio::sync::mpsc;

pub(crate) fn spawn_stdin_task() -> mpsc::Receiver<String> {
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(16);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.is_empty() {
                        continue;
                    }
                    if stdin_tx.send(line).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    eprintln!("[mlink] stdin read error: {e}");
                    return;
                }
            }
        }
    });
    stdin_rx
}

pub(crate) fn disabled_stdin() -> mpsc::Receiver<String> {
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(1);
    drop(stdin_tx);
    stdin_rx
}
