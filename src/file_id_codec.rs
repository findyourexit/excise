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
    match *file_id {
        FileId::Inode {
            device_id,
            inode_number,
        } => {
            let mut encoded = Vec::with_capacity(1 + 2 * size_of::<u64>());
            encoded.push(INODE_TAG);
            encoded.extend_from_slice(&device_id.to_le_bytes());
            encoded.extend_from_slice(&inode_number.to_le_bytes());
            encoded
        }
        FileId::LowRes {
            volume_serial_number,
            file_index,
        } => {
            let mut encoded = Vec::with_capacity(1 + size_of::<u32>() + size_of::<u64>());
            encoded.push(LOW_RES_TAG);
            encoded.extend_from_slice(&volume_serial_number.to_le_bytes());
            encoded.extend_from_slice(&file_index.to_le_bytes());
            encoded
        }
        FileId::HighRes {
            volume_serial_number,
            file_id,
        } => {
            let mut encoded = Vec::with_capacity(1 + size_of::<u64>() + size_of::<u128>());
            encoded.push(HIGH_RES_TAG);
            encoded.extend_from_slice(&volume_serial_number.to_le_bytes());
            encoded.extend_from_slice(&file_id.to_le_bytes());
            encoded
        }
    }
}

/// # Errors
///
/// Returns [`FileIdCodecError::Malformed`] when `encoded` has an unknown tag or
/// does not have the exact canonical length for that tag.
pub(crate) fn decode_file_id(encoded: &[u8]) -> Result<FileId, FileIdCodecError> {
    let (&tag, mut payload) = encoded.split_first().ok_or(FileIdCodecError::Malformed)?;
    match tag {
        INODE_TAG if payload.len() == 2 * size_of::<u64>() => Ok(FileId::new_inode(
            take_u64(&mut payload)?,
            take_u64(&mut payload)?,
        )),
        LOW_RES_TAG if payload.len() == size_of::<u32>() + size_of::<u64>() => Ok(
            FileId::new_low_res(take_u32(&mut payload)?, take_u64(&mut payload)?),
        ),
        HIGH_RES_TAG if payload.len() == size_of::<u64>() + size_of::<u128>() => Ok(
            FileId::new_high_res(take_u64(&mut payload)?, take_u128(&mut payload)?),
        ),
        _ => Err(FileIdCodecError::Malformed),
    }
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
    fn file_identity_codec_rejects_unknown_and_truncated_keys() {
        for invalid in [b"".as_slice(), b"\x09".as_slice(), b"\x00\x01".as_slice()] {
            assert_eq!(decode_file_id(invalid), Err(FileIdCodecError::Malformed));
        }
    }
}
