//! Canonical, session-local scan persistence.
//!
//! This module owns durable scan facts and lossless reduction inputs. The
//! mutable UI model must only cache queries derived from this store.

pub(crate) mod directory_summary;
pub(crate) mod identity_observation;
pub(crate) mod manifest;
pub(crate) mod path_key;
pub(crate) mod path_observation;
pub(crate) mod path_reducer;
pub(crate) mod run_file;
pub(crate) mod run_merge;
pub(crate) mod session;
pub(crate) mod summary_metrics;
