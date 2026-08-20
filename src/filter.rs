use std::ffi::OsStr;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use thiserror::Error;

use crate::native_path::{safe_display_os_str, safe_display_path};

const MAXIMUM_FILTER_CHARACTERS: usize = 256;

#[derive(Clone, Debug)]
pub struct FilterPattern {
    raw: String,
    matcher: Option<GlobMatcher>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum FilterError {
    #[error("filter cannot be empty")]
    Empty,
    #[error("filter is longer than {MAXIMUM_FILTER_CHARACTERS} characters")]
    TooLong,
    #[error("invalid glob filter: {0}")]
    InvalidGlob(String),
}

impl FilterPattern {
    /// Compiles a bounded literal or glob filter.
    ///
    /// # Errors
    /// Returns an error for empty, oversized, or invalid glob input.
    pub fn new(raw: impl Into<String>) -> Result<Self, FilterError> {
        let raw = raw.into();
        let count = raw.chars().count();
        if count == 0 {
            return Err(FilterError::Empty);
        }
        if count > MAXIMUM_FILTER_CHARACTERS {
            return Err(FilterError::TooLong);
        }
        let matcher = contains_glob_syntax(&raw)
            .then(|| {
                GlobBuilder::new(&raw)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| FilterError::InvalidGlob(error.to_string()))
            })
            .transpose()?;
        Ok(Self { raw, matcher })
    }

    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    #[must_use]
    pub fn is_glob(&self) -> bool {
        self.matcher.is_some()
    }

    #[must_use]
    pub fn matches_name(&self, name: &OsStr) -> bool {
        let name = safe_display_os_str(name).text;
        self.matcher
            .as_ref()
            .map_or_else(|| name == self.raw, |matcher| matcher.is_match(&name))
    }

    #[must_use]
    pub fn matches_path(&self, path: &Path, base: &Path) -> bool {
        let relative = path.strip_prefix(base).unwrap_or(path);
        let displayed = safe_display_path(relative).text;
        #[cfg(windows)]
        let displayed = displayed.replace("\\\\", "/");
        self.matcher.as_ref().map_or_else(
            || {
                displayed == self.raw
                    || relative
                        .file_name()
                        .is_some_and(|name| self.matches_name(name))
            },
            |matcher| matcher.is_match(&displayed),
        )
    }
}

fn contains_glob_syntax(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '*' | '?' | '[' | '{'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_filter_matches_leaf_or_relative_path() {
        let filter = FilterPattern::new("target").expect("exact filter should compile");
        assert!(filter.matches_path(Path::new("root/target"), Path::new("root")));
        assert!(!filter.matches_path(Path::new("root/not-target"), Path::new("root")));
    }

    #[test]
    fn glob_filter_respects_path_separators() {
        let filter = FilterPattern::new("build/*.o").expect("glob should compile");
        let root = Path::new("root");
        let file = root.join("build").join("main.o");
        let deep_file = root.join("build").join("deep").join("main.o");
        assert!(filter.matches_path(&file, root));
        assert!(!filter.matches_path(&deep_file, root));
    }

    #[cfg(unix)]
    #[test]
    fn exact_filter_matches_escaped_non_utf8_name() {
        use std::os::unix::ffi::OsStrExt as _;

        let name = OsStr::from_bytes(b"bad\xff");
        let displayed = safe_display_os_str(name).text;
        let filter = FilterPattern::new(displayed).expect("escaped exact filter should compile");
        assert!(filter.matches_name(name));
    }
}
