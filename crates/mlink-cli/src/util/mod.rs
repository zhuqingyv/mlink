//! Cross-cutting helpers shared by the session/ and commands/ modules.
//! Keep each submodule leaf-only — they must not depend on session/ or
//! commands/ to avoid circular visibility.

pub(crate) mod backoff;
pub(crate) mod open_url;
pub(crate) mod printing;
