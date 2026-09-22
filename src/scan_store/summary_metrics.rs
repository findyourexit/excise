use std::mem::size_of;

use thiserror::Error;

use super::path_reducer::SummaryMetrics;
use crate::model::ByteBounds;

pub(crate) const ALLOCATED_UPPER_PRESENT: u8 = 1;
pub(crate) const RECLAIMABLE_UPPER_PRESENT: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = ALLOCATED_UPPER_PRESENT | RECLAIMABLE_UPPER_PRESENT;
const FIXED_BYTES: usize = 3 * size_of::<u128>() + size_of::<u64>();

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum SummaryMetricsCodecError {
    #[error("summary metrics value is malformed")]
    Malformed,
}

#[must_use]
pub(crate) const fn summary_metrics_flags(metrics: SummaryMetrics) -> u8 {
    let mut flags = 0_u8;
    if metrics.allocated_bytes.upper.is_some() {
        flags |= ALLOCATED_UPPER_PRESENT;
    }
    if metrics.reclaimable_bytes.upper.is_some() {
        flags |= RECLAIMABLE_UPPER_PRESENT;
    }
    flags
}

/// Appends the compact metric payload after a caller-owned record header.
pub(crate) fn encode_summary_metrics_into(metrics: SummaryMetrics, output: &mut Vec<u8>) {
    let optional_bytes = metrics
        .allocated_bytes
        .upper
        .map_or(0, |_| size_of::<u128>())
        .saturating_add(
            metrics
                .reclaimable_bytes
                .upper
                .map_or(0, |_| size_of::<u128>()),
        );
    let required = FIXED_BYTES.saturating_add(optional_bytes);
    let available = output.capacity().saturating_sub(output.len());
    if available < required {
        output.reserve(required.saturating_sub(available));
    }
    output.extend_from_slice(&metrics.apparent_bytes.to_le_bytes());
    output.extend_from_slice(&metrics.allocated_bytes.lower.to_le_bytes());
    output.extend_from_slice(&metrics.reclaimable_bytes.lower.to_le_bytes());
    output.extend_from_slice(&metrics.descendants.to_le_bytes());
    if let Some(upper) = metrics.allocated_bytes.upper {
        output.extend_from_slice(&upper.to_le_bytes());
    }
    if let Some(upper) = metrics.reclaimable_bytes.upper {
        output.extend_from_slice(&upper.to_le_bytes());
    }
}

/// # Errors
///
/// Returns an error for unknown flags, truncated fields, inconsistent bounds,
/// or trailing bytes left for the caller to handle.
pub(crate) fn decode_summary_metrics(
    flags: u8,
    input: &mut &[u8],
) -> Result<SummaryMetrics, SummaryMetricsCodecError> {
    if flags & !KNOWN_FLAGS != 0 {
        return Err(SummaryMetricsCodecError::Malformed);
    }
    let apparent_bytes = take_u128(input)?;
    let allocated_lower = take_u128(input)?;
    let reclaimable_lower = take_u128(input)?;
    let descendants = take_u64(input)?;
    let allocated_upper = (flags & ALLOCATED_UPPER_PRESENT != 0)
        .then(|| take_u128(input))
        .transpose()?;
    let reclaimable_upper = (flags & RECLAIMABLE_UPPER_PRESENT != 0)
        .then(|| take_u128(input))
        .transpose()?;
    if allocated_upper.is_some_and(|upper| upper < allocated_lower)
        || reclaimable_upper.is_some_and(|upper| upper < reclaimable_lower)
    {
        return Err(SummaryMetricsCodecError::Malformed);
    }
    Ok(SummaryMetrics {
        apparent_bytes,
        allocated_bytes: ByteBounds {
            lower: allocated_lower,
            upper: allocated_upper,
        },
        reclaimable_bytes: ByteBounds {
            lower: reclaimable_lower,
            upper: reclaimable_upper,
        },
        descendants,
    })
}

fn take_u64(input: &mut &[u8]) -> Result<u64, SummaryMetricsCodecError> {
    Ok(u64::from_le_bytes(take_array(input)?))
}

fn take_u128(input: &mut &[u8]) -> Result<u128, SummaryMetricsCodecError> {
    Ok(u128::from_le_bytes(take_array(input)?))
}

fn take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], SummaryMetricsCodecError> {
    if input.len() < N {
        return Err(SummaryMetricsCodecError::Malformed);
    }
    let mut value = [0_u8; N];
    value.copy_from_slice(&input[..N]);
    *input = &input[N..];
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_metrics_codec_round_trips_known_and_unknown_bounds() {
        let metrics = SummaryMetrics {
            apparent_bytes: 9,
            allocated_bytes: ByteBounds::exact(7),
            reclaimable_bytes: ByteBounds::unknown(),
            descendants: 3,
        };
        let mut encoded = Vec::new();
        encode_summary_metrics_into(metrics, &mut encoded);
        let mut encoded = encoded.as_slice();
        assert_eq!(
            decode_summary_metrics(summary_metrics_flags(metrics), &mut encoded)
                .expect("metrics should decode"),
            metrics
        );
        assert!(encoded.is_empty());
    }

    #[test]
    fn summary_metrics_codec_rejects_unknown_flags_and_invalid_bounds() {
        let mut invalid = Vec::new();
        invalid.extend_from_slice(&0_u128.to_le_bytes());
        invalid.extend_from_slice(&5_u128.to_le_bytes());
        invalid.extend_from_slice(&0_u128.to_le_bytes());
        invalid.extend_from_slice(&0_u64.to_le_bytes());
        invalid.extend_from_slice(&4_u128.to_le_bytes());
        assert_eq!(
            decode_summary_metrics(ALLOCATED_UPPER_PRESENT, &mut invalid.as_slice()),
            Err(SummaryMetricsCodecError::Malformed)
        );
        assert_eq!(
            decode_summary_metrics(0b100, &mut [].as_slice()),
            Err(SummaryMetricsCodecError::Malformed)
        );
    }
}
