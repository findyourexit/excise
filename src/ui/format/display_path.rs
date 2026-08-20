use std::borrow::Cow;
use std::path::Path;

#[cfg(not(test))]
pub fn display_path(path: &Path) -> Cow<'_, str> {
    Cow::Owned(crate::native_path::safe_display_path(path).text)
}

#[cfg(test)]
pub fn display_path(path: &Path) -> Cow<'_, str> {
    let normalized = crate::tests::fixtures::snapshot_path(path);
    Cow::Owned(crate::native_path::safe_display_path(Path::new(&normalized)).text)
}
