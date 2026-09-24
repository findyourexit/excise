//! Private, per-session scan storage.
//!
//! This module holds durable scan facts and the data used to combine them. The
//! mutable UI model may cache only queries derived from this store.

pub(crate) mod directory_summary;
pub(crate) mod identity_observation;
pub(crate) mod manifest;
pub(crate) mod page;
pub(crate) mod path_catalog;
pub(crate) mod path_key;
pub(crate) mod path_observation;
pub(crate) mod path_reducer;
pub(crate) mod run_file;
pub(crate) mod run_merge;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod summary_metrics;
