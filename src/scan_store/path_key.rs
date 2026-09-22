use std::ffi::{OsStr, OsString};

use thiserror::Error;

use crate::scan_coordinator::{RelativePath, RelativePathError};

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum PathKeyError {
    #[error("scan path component contains a NUL code unit")]
    NulComponent,
    #[error("scan path key is malformed")]
    Malformed,
    #[error("scan path key is not valid native text on this platform")]
    WrongPlatform,
}

/// Encodes a relative native path so component-wise lexical ordering survives
/// bytewise run sorting. The root is represented by an empty key.
///
/// # Errors
///
/// Returns an error only for a component containing a native NUL code unit.
pub(crate) fn encode_path_key(path: &RelativePath) -> Result<Vec<u8>, PathKeyError> {
    let mut encoded = Vec::new();
    encode_path_key_into(path, &mut encoded)?;
    Ok(encoded)
}

/// Reuses `encoded` for a relative native path key.
///
/// # Errors
///
/// Returns an error only for a component containing a native NUL code unit.
pub(crate) fn encode_path_key_into(
    path: &RelativePath,
    encoded: &mut Vec<u8>,
) -> Result<(), PathKeyError> {
    encoded.clear();
    append_path_key(path, encoded)
}

/// Appends one native path component in the same order-preserving encoding
/// used by [`append_path_key`].
///
/// # Errors
///
/// Returns an error only when the component contains a native NUL code unit.
pub(crate) fn append_path_component_key(
    component: &OsStr,
    encoded: &mut Vec<u8>,
) -> Result<(), PathKeyError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let component = component.as_bytes();
        if component.contains(&0) {
            return Err(PathKeyError::NulComponent);
        }
        encoded.reserve(component.len().saturating_add(1));
        encoded.extend_from_slice(component);
        encoded.push(0);
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;

        let mut units = component.encode_wide();
        let length = units.clone().count();
        if units.any(|unit| unit == 0) {
            return Err(PathKeyError::NulComponent);
        }
        encoded.reserve(length.saturating_mul(2).saturating_add(2));
        for unit in component.encode_wide() {
            encoded.extend_from_slice(&unit.to_be_bytes());
        }
        encoded.extend_from_slice(&0_u16.to_be_bytes());
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let component = component.to_str().ok_or(PathKeyError::WrongPlatform)?;
        if component.as_bytes().contains(&0) {
            return Err(PathKeyError::NulComponent);
        }
        encoded.reserve(component.len().saturating_add(1));
        encoded.extend_from_slice(component.as_bytes());
        encoded.push(0);
        Ok(())
    }
}

/// Appends one component-delimited canonical path key to an existing record.
///
/// # Errors
///
/// Returns an error only for a component containing a native NUL code unit.
pub(crate) fn append_path_key(
    path: &RelativePath,
    encoded: &mut Vec<u8>,
) -> Result<(), PathKeyError> {
    for component in path.components() {
        append_path_component_key(component, encoded)?;
    }
    Ok(())
}

/// Decodes a key written by [`encode_path_key`].
///
/// # Errors
///
/// Returns an error for malformed framing or a key that is invalid for this
/// platform's native path representation.
pub(crate) fn decode_path_key(encoded: &[u8]) -> Result<RelativePath, PathKeyError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;

        let mut remaining = encoded;
        let mut components = Vec::new();
        while !remaining.is_empty() {
            let Some(length) = remaining.iter().position(|byte| *byte == 0) else {
                return Err(PathKeyError::Malformed);
            };
            if length == 0 {
                return Err(PathKeyError::Malformed);
            }
            components.push(OsString::from_vec(remaining[..length].to_vec()));
            remaining = &remaining[length.saturating_add(1)..];
        }
        RelativePath::from_components(components).map_err(map_relative_path_error)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;

        if !encoded.len().is_multiple_of(2) {
            return Err(PathKeyError::Malformed);
        }
        let mut components = Vec::new();
        let mut component = Vec::new();
        for &[high, low] in encoded.as_chunks::<2>().0 {
            let unit = u16::from_be_bytes([high, low]);
            if unit == 0 {
                if component.is_empty() {
                    return Err(PathKeyError::Malformed);
                }
                components.push(OsString::from_wide(&component));
                component.clear();
            } else {
                component.push(unit);
            }
        }
        if !component.is_empty() {
            return Err(PathKeyError::Malformed);
        }
        RelativePath::from_components(components).map_err(map_relative_path_error)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let mut remaining = encoded;
        let mut components = Vec::new();
        while !remaining.is_empty() {
            let Some(length) = remaining.iter().position(|byte| *byte == 0) else {
                return Err(PathKeyError::Malformed);
            };
            if length == 0 {
                return Err(PathKeyError::Malformed);
            }
            let component = std::str::from_utf8(&remaining[..length])
                .map_err(|_| PathKeyError::WrongPlatform)?;
            components.push(OsString::from(component));
            remaining = &remaining[length.saturating_add(1)..];
        }
        RelativePath::from_components(components).map_err(map_relative_path_error)
    }
}

fn map_relative_path_error(_error: RelativePathError) -> PathKeyError {
    PathKeyError::Malformed
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    #[test]
    fn path_keys_round_trip_root_and_nested_native_components() {
        for path in [
            RelativePath::root(),
            path("alpha"),
            path("alpha/beta"),
            path("zeta"),
        ] {
            let encoded = encode_path_key(&path).expect("path key should encode");
            assert_eq!(
                decode_path_key(&encoded).expect("path key should decode"),
                path
            );
        }
    }

    #[test]
    fn path_key_order_matches_component_order() {
        let mut paths = vec![
            path("zeta"),
            path("alpha/beta"),
            path("alpha"),
            path("alphabet"),
            RelativePath::root(),
        ];
        let mut encoded = paths
            .iter()
            .cloned()
            .map(|path| (encode_path_key(&path).expect("path should encode"), path))
            .collect::<Vec<_>>();
        paths.sort();
        encoded.sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(
            encoded
                .into_iter()
                .map(|(_, path)| path)
                .collect::<Vec<_>>(),
            paths
        );
    }

    #[test]
    fn decoder_rejects_unterminated_or_empty_components() {
        assert_eq!(decode_path_key(b"alpha"), Err(PathKeyError::Malformed));
        assert_eq!(decode_path_key(b"\0"), Err(PathKeyError::Malformed));
        assert_eq!(decode_path_key(b"alpha\0\0"), Err(PathKeyError::Malformed));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unix_components_round_trip_without_loss() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = RelativePath::from_components(vec![OsString::from_vec(vec![0xff, b'a'])])
            .expect("non-UTF-8 component should remain a relative path");
        let encoded = encode_path_key(&path).expect("non-UTF-8 component should encode");
        assert_eq!(
            decode_path_key(&encoded).expect("non-UTF-8 component should decode"),
            path
        );
    }
}
