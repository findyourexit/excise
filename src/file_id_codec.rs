use std::mem::size_of;

use file_id::FileId;
use thiserror::Error;

const INODE_TAG: u8 = 0;
const LOW_RES_TAG: u8 = 1;
const HIGH_RES_TAG: u8 = 2;

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum FileIdCodecError {
    #[error("file identity key is malformed")]
    Malformed,
}

/// Encodes a file identity without JSON allocation or platform-dependent text.
#[must_use]
pub(crate) fn encode_file_id(file_id: &FileId) -> Vec<u8> {
    let mut encoded = Vec::new();
    encode_file_id_into(file_id, &mut encoded);
    encoded
}

/// Reuses `encoded` for one canonical file identity.
pub(crate) fn encode_file_id_into(file_id: &FileId, encoded: &mut Vec<u8>) {
    encoded.clear();
    match *file_id {
        FileId::Inode {
            device_id,
            inode_number,
        } => {
            encoded.reserve(1 + 2 * size_of::<u64>());
            encoded.push(INODE_TAG);
            encoded.extend_from_slice(&device_id.to_le_bytes());
            encoded.extend_from_slice(&inode_number.to_le_bytes());
        }
        FileId::LowRes {
            volume_serial_number,
            file_index,
        } => {
            encoded.reserve(1 + size_of::<u32>() + size_of::<u64>());
            encoded.push(LOW_RES_TAG);
            encoded.extend_from_slice(&volume_serial_number.to_le_bytes());
            encoded.extend_from_slice(&file_index.to_le_bytes());
        }
        FileId::HighRes {
            volume_serial_number,
            file_id,
        } => {
            encoded.reserve(1 + size_of::<u64>() + size_of::<u128>());
            encoded.push(HIGH_RES_TAG);
            encoded.extend_from_slice(&volume_serial_number.to_le_bytes());
            encoded.extend_from_slice(&file_id.to_le_bytes());
        }
    }
}

/// Decodes one canonical file identity at the start of `encoded`.
///
/// # Errors
///
/// Returns [`FileIdCodecError::Malformed`] when `encoded` starts with an
/// unknown tag or lacks the complete canonical bytes for that tag.
pub(crate) fn decode_file_id_prefix(encoded: &[u8]) -> Result<(FileId, usize), FileIdCodecError> {
    let (&tag, _) = encoded.split_first().ok_or(FileIdCodecError::Malformed)?;
    let payload_bytes = match tag {
        INODE_TAG => 2 * size_of::<u64>(),
        LOW_RES_TAG => size_of::<u32>() + size_of::<u64>(),
        HIGH_RES_TAG => size_of::<u64>() + size_of::<u128>(),
        _ => return Err(FileIdCodecError::Malformed),
    };
    let total_bytes = payload_bytes.saturating_add(1);
    if encoded.len() < total_bytes {
        return Err(FileIdCodecError::Malformed);
    }
    let mut payload = &encoded[1..total_bytes];
    let file_id = match tag {
        INODE_TAG => FileId::new_inode(take_u64(&mut payload)?, take_u64(&mut payload)?),
        LOW_RES_TAG => FileId::new_low_res(take_u32(&mut payload)?, take_u64(&mut payload)?),
        HIGH_RES_TAG => FileId::new_high_res(take_u64(&mut payload)?, take_u128(&mut payload)?),
        _ => unreachable!("the tag was validated before decoding"),
    };
    debug_assert!(payload.is_empty());
    Ok((file_id, total_bytes))
}

/// # Errors
///
/// Returns [`FileIdCodecError::Malformed`] when `encoded` has an unknown tag or
/// does not have the exact canonical length for that tag.
pub(crate) fn decode_file_id(encoded: &[u8]) -> Result<FileId, FileIdCodecError> {
    let (file_id, consumed) = decode_file_id_prefix(encoded)?;
    if consumed != encoded.len() {
        return Err(FileIdCodecError::Malformed);
    }
    Ok(file_id)
}

fn take_u32(input: &mut &[u8]) -> Result<u32, FileIdCodecError> {
    Ok(u32::from_le_bytes(take_array(input)?))
}

fn take_u64(input: &mut &[u8]) -> Result<u64, FileIdCodecError> {
    Ok(u64::from_le_bytes(take_array(input)?))
}

fn take_u128(input: &mut &[u8]) -> Result<u128, FileIdCodecError> {
    Ok(u128::from_le_bytes(take_array(input)?))
}

fn take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], FileIdCodecError> {
    if input.len() < N {
        return Err(FileIdCodecError::Malformed);
    }
    let mut bytes = [0_u8; N];
    bytes.copy_from_slice(&input[..N]);
    *input = &input[N..];
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_identity_codec_round_trips_every_supported_variant() {
        for file_id in [
            FileId::new_inode(1, 2),
            FileId::new_low_res(3, 4),
            FileId::new_high_res(5, 6),
        ] {
            assert_eq!(
                decode_file_id(&encode_file_id(&file_id)).expect("identity should decode"),
                file_id
            );
        }
    }

    #[test]
    fn file_identity_prefix_decoder_preserves_a_following_key() {
        let file_id = FileId::new_low_res(7, 9);
        let mut encoded = encode_file_id(&file_id);
        let identity_bytes = encoded.len();
        encoded.extend_from_slice(b"child\0");
        assert_eq!(
            decode_file_id_prefix(&encoded).expect("identity prefix should decode"),
            (file_id, identity_bytes)
        );
        assert_eq!(decode_file_id(&encoded), Err(FileIdCodecError::Malformed));
    }

    #[test]
    fn file_identity_codec_rejects_unknown_and_truncated_keys() {
        for invalid in [b"".as_slice(), b"\x09".as_slice(), b"\x00\x01".as_slice()] {
            assert_eq!(decode_file_id(invalid), Err(FileIdCodecError::Malformed));
        }
    }
}
