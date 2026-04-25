//! Shared session-level plumbing used by the serve / chat / join commands.
//! Each submodule captures one piece of the otherwise-duplicated `cmd_*`
//! skeletons — scanner, listener, stdin, dial/accept — so the command
//! modules can focus on control flow instead of re-implementing the same
//! tokio::spawn + mpsc dance three times.

pub(crate) mod dial;
pub(crate) mod listen;
pub(crate) mod scanner;
pub(crate) mod stdin;
