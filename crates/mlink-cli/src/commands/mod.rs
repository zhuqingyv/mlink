//! One module per CLI subcommand. Long-running `serve` / `chat` / `join`
//! live alongside the legacy peer-id commands and the room / send helpers.
//! The `run.rs` dispatcher imports from here and maps each `Commands::*`
//! variant to its corresponding `cmd_*` function.

pub(crate) mod chat;
pub(crate) mod connect;
pub(crate) mod daemon;
pub(crate) mod dev;
pub(crate) mod doctor;
pub(crate) mod join;
pub(crate) mod listen;
pub(crate) mod ping;
pub(crate) mod room;
pub(crate) mod scan;
pub(crate) mod send;
pub(crate) mod serve;
pub(crate) mod status;
pub(crate) mod trust;
