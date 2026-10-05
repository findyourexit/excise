//! Fixture tests. Everything here generates small trees into a private temporary directory and
//! passes that directory to the cache, never the shared one.

pub(crate) mod support;

mod cache;
mod classes;
#[cfg(unix)]
mod du;
mod mutate;
mod oracle;
mod readme;
#[cfg(unix)]
mod resolve;
mod run;
mod spec_files;
