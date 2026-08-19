use std::borrow::Cow;
use std::path::Path;

#[cfg(not(test))]
pub fn display_path(path: &Path) -> Cow<'_, str> {
    path.to_string_lossy()
}

#[cfg(test)]
pub fn display_path(path: &Path) -> Cow<'_, str> {
    Cow::Owned(crate::tests::fixtures::snapshot_path(path))
}
