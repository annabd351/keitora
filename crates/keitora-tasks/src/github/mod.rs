//! GitHub-backed `TaskBackend` plumbing.
//!
//! This module hosts the injectable `gh`-CLI transport seam ([`transport`]),
//! the raw-output → [`crate::BackendError`] classifier, and the retry helper
//! ([`retry`]). It deliberately contains **no** `GitHubBackend` yet — that lands
//! in a later stage and is built on top of these primitives.
//!
//! Everything here is `pub(crate)`: the GitHub types are an implementation
//! detail of the crate and are not re-exported from the crate root.

pub(crate) mod backend;
pub(crate) mod retry;
pub(crate) mod sidecar;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) mod test_support;
