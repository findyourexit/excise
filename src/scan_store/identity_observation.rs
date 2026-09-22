use std::cmp::Ordering;
use std::mem::size_of;

use file_id::FileId;
use thiserror::Error;

use super::path_key::{PathKeyError, append_path_key, decode_path_key};
use super::run_file::{RunError, RunKind, RunReader, RunWriter};
use crate::file_id_codec::{
    FileIdCodecError, compare_file_ids_by_encoding, decode_file_id, decode_file_id_prefix,
    encode_file_id_into,
};
use crate::model::ByteBounds;
use crate::scan_coordinator::RelativePath;

const IDENTITY_OBSERVATION_VERSION: u8 = 1;
const OBSERVATION_DECLARED_LINKS_PRESENT: u8 = 1;
const OBSERVATION_ALLOCATION_UPPER_PRESENT: u8 = 1 << 1;
const OBSERVATION_KNOWN_FLAGS: u8 =
    OBSERVATION_DECLARED_LINKS_PRESENT | OBSERVATION_ALLOCATION_UPPER_PRESENT;

const ALLOCATION_CONTRIBUTION_VERSION: u8 = 1;
const CONTRIBUTION_ALLOCATION_UPPER_PRESENT: u8 = 1;
const CONTRIBUTION_RECLAIMABLE_UPPER_PRESENT: u8 = 1 << 1;
const CONTRIBUTION_KNOWN_FLAGS: u8 =
    CONTRIBUTION_ALLOCATION_UPPER_PRESENT | CONTRIBUTION_RECLAIMABLE_UPPER_PRESENT;
const PATH_LENGTH_BYTES: usize = size_of::<u32>();

/// Per-path facts grouped by file identity after external sorting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IdentityObservation {
    pub(crate) path: RelativePath,
    pub(crate) file_id: FileId,
    pub(crate) declared_links: Option<u64>,
    pub(crate) allocated_bytes: ByteBounds,
}

/// Compares observations by their exact identity-observation run key.
#[must_use]
pub(crate) fn compare_identity_observations(
    left: &IdentityObservation,
    right: &IdentityObservation,
) -> Ordering {
    compare_file_ids_by_encoding(&left.file_id, &right.file_id)
        .then_with(|| left.path.cmp(&right.path))
}

/// Where a once-per-identity physical allocation belongs in the presentation tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocationPlacement {
    Leaf,
    Shared,
}

impl AllocationPlacement {
    const fn code(self) -> u8 {
        match self {
            Self::Leaf => 1,
            Self::Shared => 2,
        }
    }

    const fn from_code(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Leaf),
            2 => Some(Self::Shared),
            _ => None,
        }
    }
}

/// One identity-unique physical allocation destined for a concrete leaf or a
/// virtual shared child at its componentwise lowest common ancestor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AllocationContribution {
    pub(crate) recipient: RelativePath,
    pub(crate) placement: AllocationPlacement,
    pub(crate) allocated_bytes: ByteBounds,
    pub(crate) reclaimable_bytes: ByteBounds,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum IdentityObservationCodecError {
    #[error(transparent)]
    FileId(#[from] FileIdCodecError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error("identity observation cannot represent the scan root")]
    RootPath,
    #[error("identity observation bounds are inconsistent")]
    InconsistentBounds,
    #[error("identity observation value is malformed")]
    Malformed,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum AllocationContributionCodecError {
    #[error(transparent)]
    FileId(#[from] FileIdCodecError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error("leaf allocation contribution cannot target the scan root")]
    RootLeaf,
    #[error("allocation contribution bounds are inconsistent")]
    InconsistentBounds,
    #[error("allocation contribution value is malformed")]
    Malformed,
}

#[derive(Debug, Error)]
pub(crate) enum IdentityObservationRunError {
    #[error(transparent)]
    Codec(#[from] IdentityObservationCodecError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("identity observations require an identity-observation run")]
    WrongRunKind,
}

#[derive(Debug, Error)]
pub(crate) enum AllocationContributionRunError {
    #[error(transparent)]
    Codec(#[from] AllocationContributionCodecError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("allocation contributions require an allocation-contribution run")]
    WrongRunKind,
}

#[derive(Debug, Error)]
pub(crate) enum IdentityReductionError {
    #[error(transparent)]
    InputCodec(#[from] IdentityObservationCodecError),
    #[error(transparent)]
    OutputCodec(#[from] AllocationContributionCodecError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("identity reduction input must be an identity-observation run")]
    WrongInputKind,
    #[error("identity reduction output must be an allocation-contribution run")]
    WrongOutputKind,
    #[error("identity reduction runs must belong to the same scan generation")]
    GenerationMismatch,
    #[error("identity observation count overflow")]
    ObservationCountOverflow,
}

/// Reuses record buffers to encode one file-identity-sorted observation.
///
/// The key contains the exact canonical file identity followed by a canonical
/// path key. The value retains only facts that cannot be recovered from either.
///
/// # Errors
///
/// Returns an error when the path is root-relative invalid, cannot be encoded,
/// or the observation's bounds are inconsistent.
pub(crate) fn encode_identity_observation_into(
    observation: &IdentityObservation,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), IdentityObservationCodecError> {
    if observation.path.is_root() {
        return Err(IdentityObservationCodecError::RootPath);
    }
    validate_bounds(observation.allocated_bytes)
        .map_err(|()| IdentityObservationCodecError::InconsistentBounds)?;

    if observation.declared_links == Some(0) {
        return Err(IdentityObservationCodecError::Malformed);
    }

    encode_file_id_into(&observation.file_id, key);
    append_path_key(&observation.path, key)?;
    value.clear();
    let mut flags = 0_u8;
    if observation.declared_links.is_some() {
        flags |= OBSERVATION_DECLARED_LINKS_PRESENT;
    }
    if observation.allocated_bytes.upper.is_some() {
        flags |= OBSERVATION_ALLOCATION_UPPER_PRESENT;
    }
    value.push(IDENTITY_OBSERVATION_VERSION);
    value.push(flags);
    if let Some(declared_links) = observation.declared_links {
        value.extend_from_slice(&declared_links.to_le_bytes());
    }
    value.extend_from_slice(&observation.allocated_bytes.lower.to_le_bytes());
    if let Some(upper) = observation.allocated_bytes.upper {
        value.extend_from_slice(&upper.to_le_bytes());
    }
    Ok(())
}

/// # Errors
///
/// Returns an error for a malformed file identity/path key or malformed value.
pub(crate) fn decode_identity_observation(
    key: &[u8],
    mut value: &[u8],
) -> Result<IdentityObservation, IdentityObservationCodecError> {
    let (file_id, file_id_bytes) = decode_file_id_prefix(key)?;
    let path = decode_path_key(&key[file_id_bytes..])?;
    if path.is_root() {
        return Err(IdentityObservationCodecError::RootPath);
    }
    if take_u8::<IdentityObservationCodecError>(&mut value)? != IDENTITY_OBSERVATION_VERSION {
        return Err(IdentityObservationCodecError::Malformed);
    }
    let flags = take_u8::<IdentityObservationCodecError>(&mut value)?;
    if flags & !OBSERVATION_KNOWN_FLAGS != 0 {
        return Err(IdentityObservationCodecError::Malformed);
    }
    let declared_links = (flags & OBSERVATION_DECLARED_LINKS_PRESENT != 0)
        .then(|| take_u64::<IdentityObservationCodecError>(&mut value))
        .transpose()?;
    if declared_links == Some(0) {
        return Err(IdentityObservationCodecError::Malformed);
    }
    let lower = take_u128::<IdentityObservationCodecError>(&mut value)?;
    let upper = (flags & OBSERVATION_ALLOCATION_UPPER_PRESENT != 0)
        .then(|| take_u128::<IdentityObservationCodecError>(&mut value))
        .transpose()?;
    if !value.is_empty() {
        return Err(IdentityObservationCodecError::Malformed);
    }
    let allocated_bytes = ByteBounds { lower, upper };
    validate_bounds(allocated_bytes).map_err(|()| IdentityObservationCodecError::Malformed)?;

    Ok(IdentityObservation {
        path,
        file_id,
        declared_links,
        allocated_bytes,
    })
}

/// Encodes and appends one identity observation to an identity-observation run.
///
/// # Errors
///
/// Returns an error when the run kind is wrong or encoding/writing fails.
pub(crate) fn append_identity_observation(
    writer: &mut RunWriter,
    observation: &IdentityObservation,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), IdentityObservationRunError> {
    if writer.descriptor().kind() != RunKind::IdentityObservation {
        return Err(IdentityObservationRunError::WrongRunKind);
    }
    encode_identity_observation_into(observation, key, value)?;
    writer.append(key, value)?;
    Ok(())
}

/// Visits verified identity observations without retaining the run contents.
///
/// # Errors
///
/// Returns an error when the run kind, framing, or observation codec is invalid.
pub(crate) fn visit_identity_observations(
    reader: &mut RunReader,
    mut visit: impl FnMut(IdentityObservation) -> Result<(), IdentityObservationRunError>,
) -> Result<(), IdentityObservationRunError> {
    if reader.descriptor().kind() != RunKind::IdentityObservation {
        return Err(IdentityObservationRunError::WrongRunKind);
    }
    let mut key = Vec::new();
    let mut value = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        visit(decode_identity_observation(&key, &value)?)?;
    }
    Ok(())
}

/// Reuses record buffers to encode one identity-unique allocation contribution.
///
/// The key is exactly the contributing canonical file identity; the value holds
/// its destination path and accounting outcome.
///
/// # Errors
///
/// Returns an error when a leaf targets the root, a path cannot be encoded, or
/// bounds are inconsistent.
pub(crate) fn encode_allocation_contribution_into(
    file_id: &FileId,
    contribution: &AllocationContribution,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), AllocationContributionCodecError> {
    if contribution.placement == AllocationPlacement::Leaf && contribution.recipient.is_root() {
        return Err(AllocationContributionCodecError::RootLeaf);
    }
    validate_bounds(contribution.allocated_bytes)
        .map_err(|()| AllocationContributionCodecError::InconsistentBounds)?;

    validate_bounds(contribution.reclaimable_bytes)
        .map_err(|()| AllocationContributionCodecError::InconsistentBounds)?;

    encode_file_id_into(file_id, key);
    value.clear();
    let mut flags = 0_u8;
    if contribution.allocated_bytes.upper.is_some() {
        flags |= CONTRIBUTION_ALLOCATION_UPPER_PRESENT;
    }
    if contribution.reclaimable_bytes.upper.is_some() {
        flags |= CONTRIBUTION_RECLAIMABLE_UPPER_PRESENT;
    }
    value.push(ALLOCATION_CONTRIBUTION_VERSION);
    value.push(contribution.placement.code());
    value.push(flags);
    let path_length_offset = value.len();
    value.extend_from_slice(&[0_u8; PATH_LENGTH_BYTES]);
    let path_start = value.len();
    append_path_key(&contribution.recipient, value)?;
    let path_length = u32::try_from(value.len().saturating_sub(path_start))
        .map_err(|_| AllocationContributionCodecError::Malformed)?;
    value[path_length_offset..path_start].copy_from_slice(&path_length.to_le_bytes());
    value.extend_from_slice(&contribution.allocated_bytes.lower.to_le_bytes());
    value.extend_from_slice(&contribution.reclaimable_bytes.lower.to_le_bytes());
    if let Some(upper) = contribution.allocated_bytes.upper {
        value.extend_from_slice(&upper.to_le_bytes());
    }
    if let Some(upper) = contribution.reclaimable_bytes.upper {
        value.extend_from_slice(&upper.to_le_bytes());
    }
    Ok(())
}

/// # Errors
///
/// Returns an error for malformed identity keys, placement, paths, or bounds.
pub(crate) fn decode_allocation_contribution(
    key: &[u8],
    mut value: &[u8],
) -> Result<(FileId, AllocationContribution), AllocationContributionCodecError> {
    let file_id = decode_file_id(key)?;
    if take_u8::<AllocationContributionCodecError>(&mut value)? != ALLOCATION_CONTRIBUTION_VERSION {
        return Err(AllocationContributionCodecError::Malformed);
    }
    let placement =
        AllocationPlacement::from_code(take_u8::<AllocationContributionCodecError>(&mut value)?)
            .ok_or(AllocationContributionCodecError::Malformed)?;
    let flags = take_u8::<AllocationContributionCodecError>(&mut value)?;
    if flags & !CONTRIBUTION_KNOWN_FLAGS != 0 {
        return Err(AllocationContributionCodecError::Malformed);
    }
    let path_length = usize::try_from(take_u32::<AllocationContributionCodecError>(&mut value)?)
        .map_err(|_| AllocationContributionCodecError::Malformed)?;
    let recipient = decode_path_key(take_bytes::<AllocationContributionCodecError>(
        &mut value,
        path_length,
    )?)?;
    if placement == AllocationPlacement::Leaf && recipient.is_root() {
        return Err(AllocationContributionCodecError::RootLeaf);
    }
    let allocated_lower = take_u128::<AllocationContributionCodecError>(&mut value)?;
    let reclaimable_lower = take_u128::<AllocationContributionCodecError>(&mut value)?;
    let allocated_upper = (flags & CONTRIBUTION_ALLOCATION_UPPER_PRESENT != 0)
        .then(|| take_u128::<AllocationContributionCodecError>(&mut value))
        .transpose()?;
    let reclaimable_upper = (flags & CONTRIBUTION_RECLAIMABLE_UPPER_PRESENT != 0)
        .then(|| take_u128::<AllocationContributionCodecError>(&mut value))
        .transpose()?;
    if !value.is_empty() {
        return Err(AllocationContributionCodecError::Malformed);
    }
    let allocated_bytes = ByteBounds {
        lower: allocated_lower,
        upper: allocated_upper,
    };
    let reclaimable_bytes = ByteBounds {
        lower: reclaimable_lower,
        upper: reclaimable_upper,
    };
    validate_bounds(allocated_bytes).map_err(|()| AllocationContributionCodecError::Malformed)?;
    validate_bounds(reclaimable_bytes).map_err(|()| AllocationContributionCodecError::Malformed)?;
    Ok((
        file_id,
        AllocationContribution {
            recipient,
            placement,
            allocated_bytes,
            reclaimable_bytes,
        },
    ))
}

/// Appends one allocation contribution to an allocation-contribution run.
///
/// # Errors
///
/// Returns an error when the run kind is wrong or encoding/writing fails.
pub(crate) fn append_allocation_contribution(
    writer: &mut RunWriter,
    file_id: &FileId,
    contribution: &AllocationContribution,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), AllocationContributionRunError> {
    if writer.descriptor().kind() != RunKind::AllocationContribution {
        return Err(AllocationContributionRunError::WrongRunKind);
    }
    encode_allocation_contribution_into(file_id, contribution, key, value)?;
    writer.append(key, value)?;
    Ok(())
}

/// Visits verified allocation contributions without retaining the run contents.
///
/// # Errors
///
/// Returns an error when the run kind, framing, or contribution codec is invalid.
pub(crate) fn visit_allocation_contributions(
    reader: &mut RunReader,
    mut visit: impl FnMut(FileId, AllocationContribution) -> Result<(), AllocationContributionRunError>,
) -> Result<(), AllocationContributionRunError> {
    if reader.descriptor().kind() != RunKind::AllocationContribution {
        return Err(AllocationContributionRunError::WrongRunKind);
    }
    let mut key = Vec::new();
    let mut value = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        let (file_id, contribution) = decode_allocation_contribution(&key, &value)?;
        visit(file_id, contribution)?;
    }
    Ok(())
}

/// Streams a sorted identity-observation run into one record per unique file
/// identity. It retains only the current group's scalar facts and common path.
///
/// # Errors
///
/// Returns an error for mismatched run descriptors, malformed records, or run
/// I/O. Physical allocations are intentionally downgraded to unknown instead
/// of guessed when the same identity reports differing/unknown bounds.
pub(crate) fn reduce_identity_observations(
    input: &mut RunReader,
    output: &mut RunWriter,
) -> Result<(), IdentityReductionError> {
    if input.descriptor().kind() != RunKind::IdentityObservation {
        return Err(IdentityReductionError::WrongInputKind);
    }
    if output.descriptor().kind() != RunKind::AllocationContribution {
        return Err(IdentityReductionError::WrongOutputKind);
    }
    if input.descriptor().generation() != output.descriptor().generation() {
        return Err(IdentityReductionError::GenerationMismatch);
    }

    let mut input_key = Vec::new();
    let mut input_value = Vec::new();
    let mut output_key = Vec::new();
    let mut output_value = Vec::new();
    let mut group: Option<IdentityGroup> = None;
    while input.next_record_into(&mut input_key, &mut input_value)? {
        let observation = decode_identity_observation(&input_key, &input_value)?;
        match group.as_mut() {
            Some(current) if current.file_id == observation.file_id => {
                current.observe(&observation)?;
            }
            Some(_) => {
                flush_group(
                    group.take().expect("identity group was matched above"),
                    output,
                    &mut output_key,
                    &mut output_value,
                )?;
                group = Some(IdentityGroup::from_observation(observation));
            }
            None => group = Some(IdentityGroup::from_observation(observation)),
        }
    }
    if let Some(group) = group {
        flush_group(group, output, &mut output_key, &mut output_value)?;
    }
    Ok(())
}

struct IdentityGroup {
    file_id: FileId,
    recipient: RelativePath,
    observed_links: u64,
    declared_links: Option<u64>,
    exact_allocated_bytes: Option<u128>,
}

impl IdentityGroup {
    fn from_observation(observation: IdentityObservation) -> Self {
        let exact_allocated_bytes = exact_bytes(observation.allocated_bytes);
        Self {
            file_id: observation.file_id,
            recipient: observation.path,
            observed_links: 1,
            declared_links: observation.declared_links,
            exact_allocated_bytes,
        }
    }

    fn observe(&mut self, observation: &IdentityObservation) -> Result<(), IdentityReductionError> {
        self.observed_links = self
            .observed_links
            .checked_add(1)
            .ok_or(IdentityReductionError::ObservationCountOverflow)?;
        self.declared_links = merge_declared_links(self.declared_links, observation.declared_links);
        self.exact_allocated_bytes = match (
            self.exact_allocated_bytes,
            exact_bytes(observation.allocated_bytes),
        ) {
            (Some(current), Some(observed)) if current == observed => Some(current),
            _ => None,
        };
        self.recipient = common_ancestor(&self.recipient, &observation.path);
        Ok(())
    }

    fn contribution(self) -> AllocationContribution {
        let allocated_bytes = self
            .exact_allocated_bytes
            .map_or_else(ByteBounds::unknown, ByteBounds::exact);
        let reclaimable_bytes = if self
            .declared_links
            .is_some_and(|declared| self.observed_links >= declared)
        {
            allocated_bytes
        } else {
            ByteBounds {
                lower: 0,
                upper: allocated_bytes.upper,
            }
        };
        AllocationContribution {
            recipient: self.recipient,
            placement: if self.observed_links == 1 {
                AllocationPlacement::Leaf
            } else {
                AllocationPlacement::Shared
            },
            allocated_bytes,
            reclaimable_bytes,
        }
    }
}

fn flush_group(
    group: IdentityGroup,
    output: &mut RunWriter,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), IdentityReductionError> {
    let file_id = group.file_id;
    let contribution = group.contribution();
    append_allocation_contribution(output, &file_id, &contribution, key, value).map_err(|error| {
        match error {
            AllocationContributionRunError::Codec(error) => {
                IdentityReductionError::OutputCodec(error)
            }
            AllocationContributionRunError::Run(error) => IdentityReductionError::Run(error),
            AllocationContributionRunError::WrongRunKind => IdentityReductionError::WrongOutputKind,
        }
    })
}

fn common_ancestor(left: &RelativePath, right: &RelativePath) -> RelativePath {
    let shared = left
        .components()
        .iter()
        .zip(right.components())
        .take_while(|(left, right)| left == right)
        .count();
    RelativePath::from_components(left.components()[..shared].to_vec())
        .expect("a prefix of a valid relative path remains valid")
}

fn exact_bytes(bounds: ByteBounds) -> Option<u128> {
    (bounds.upper == Some(bounds.lower)).then_some(bounds.lower)
}

fn merge_declared_links(current: Option<u64>, observed: Option<u64>) -> Option<u64> {
    match (current, observed) {
        (Some(current), Some(observed)) if current == observed => Some(current),
        _ => None,
    }
}

fn validate_bounds(bounds: ByteBounds) -> Result<(), ()> {
    if bounds.upper.is_some_and(|upper| upper < bounds.lower) {
        return Err(());
    }
    Ok(())
}

#[derive(Debug)]
struct WireMalformed;

impl From<WireMalformed> for IdentityObservationCodecError {
    fn from(_: WireMalformed) -> Self {
        Self::Malformed
    }
}

impl From<WireMalformed> for AllocationContributionCodecError {
    fn from(_: WireMalformed) -> Self {
        Self::Malformed
    }
}

fn take_u8<E>(input: &mut &[u8]) -> Result<u8, E>
where
    E: From<WireMalformed>,
{
    let (&value, remainder) = input.split_first().ok_or_else(|| E::from(WireMalformed))?;
    *input = remainder;
    Ok(value)
}

fn take_u32<E>(input: &mut &[u8]) -> Result<u32, E>
where
    E: From<WireMalformed>,
{
    Ok(u32::from_le_bytes(take_array::<4, E>(input)?))
}

fn take_u64<E>(input: &mut &[u8]) -> Result<u64, E>
where
    E: From<WireMalformed>,
{
    Ok(u64::from_le_bytes(take_array::<8, E>(input)?))
}

fn take_u128<E>(input: &mut &[u8]) -> Result<u128, E>
where
    E: From<WireMalformed>,
{
    Ok(u128::from_le_bytes(take_array::<16, E>(input)?))
}

fn take_bytes<'a, E>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8], E>
where
    E: From<WireMalformed>,
{
    if input.len() < count {
        return Err(E::from(WireMalformed));
    }
    let (taken, remainder) = input.split_at(count);
    *input = remainder;
    Ok(taken)
}

fn take_array<const N: usize, E>(input: &mut &[u8]) -> Result<[u8; N], E>
where
    E: From<WireMalformed>,
{
    let bytes = take_bytes::<E>(input, N)?;
    let mut value = [0_u8; N];
    value.copy_from_slice(bytes);
    Ok(value)
}

/// Exercises the canonical identity-run reducer from the fuzz harness.
///
/// The fixture constructs valid, identity-sorted observations with repeated
/// identities and mixed known/unknown bounds, then round-trips them through
/// the same run codec and streaming reducer used by a production publication.
#[cfg(feature = "fuzzing")]
#[must_use]
pub(crate) fn fuzz_reduce_identity_bytes(data: &[u8]) -> usize {
    use std::path::Path;

    use crate::scan_coordinator::ScanGeneration;
    use crate::scan_store::run_file::RunDescriptor;
    use crate::temporary_storage::TemporaryStorage;

    let storage = TemporaryStorage::from_mib(2).expect("minimum fuzz storage should fit");
    let mut observations = Vec::with_capacity(data.len().saturating_add(4) / 5);
    for (index, chunk) in data.chunks(5).take(64).enumerate() {
        let device = u64::from(chunk.first().copied().unwrap_or_default() % 4);
        let inode = u64::from(chunk.get(1).copied().unwrap_or_default() % 16);
        let lower = u128::from(chunk.get(2).copied().unwrap_or_default());
        let upper = (chunk.get(3).copied().unwrap_or_default() & 1 == 0)
            .then(|| lower.saturating_add(u128::from(chunk.get(4).copied().unwrap_or_default())));
        let declared_links = (chunk.get(4).copied().unwrap_or_default() & 1 == 0)
            .then_some(u64::from(chunk.get(4).copied().unwrap_or_default() % 8 + 1));
        let path = format!("branch-{device}/entry-{index}");
        observations.push(IdentityObservation {
            path: RelativePath::from_path(Path::new(&path))
                .expect("generated fuzz path should be relative"),
            file_id: FileId::new_inode(device, inode),
            declared_links,
            allocated_bytes: ByteBounds { lower, upper },
        });
    }
    observations.sort_unstable_by(compare_identity_observations);

    let mut input = RunWriter::new(
        tempfile::tempfile().expect("fuzz input run should open"),
        storage
            .reservation(0)
            .expect("fuzz input reservation should fit"),
        RunDescriptor::new(ScanGeneration::initial(), 1, RunKind::IdentityObservation),
        512,
    )
    .expect("fuzz input writer should initialize");
    let mut key = Vec::new();
    let mut value = Vec::new();
    for observation in &observations {
        append_identity_observation(&mut input, observation, &mut key, &mut value)
            .expect("generated fuzz observation should append");
    }
    let mut input = input
        .seal()
        .expect("fuzz input should seal")
        .into_reader()
        .expect("fuzz input should reopen");
    let mut output = RunWriter::new(
        tempfile::tempfile().expect("fuzz output run should open"),
        storage
            .reservation(0)
            .expect("fuzz output reservation should fit"),
        RunDescriptor::new(
            ScanGeneration::initial(),
            2,
            RunKind::AllocationContribution,
        ),
        512,
    )
    .expect("fuzz output writer should initialize");
    reduce_identity_observations(&mut input, &mut output)
        .expect("generated fuzz observations should reduce");
    let mut output = output
        .seal()
        .expect("fuzz output should seal")
        .into_reader()
        .expect("fuzz output should reopen");
    let mut contributions = 0_usize;
    visit_allocation_contributions(&mut output, |_, _| {
        contributions = contributions.saturating_add(1);
        Ok::<(), AllocationContributionRunError>(())
    })
    .expect("fuzz contributions should decode");
    contributions
}
#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::scan_coordinator::ScanGeneration;
    use crate::scan_store::run_file::RunDescriptor;
    use crate::temporary_storage::TemporaryStorage;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn observation(
        path_text: &str,
        file_id: FileId,
        declared_links: Option<u64>,
        allocated_bytes: ByteBounds,
    ) -> IdentityObservation {
        IdentityObservation {
            path: path(path_text),
            file_id,
            declared_links,
            allocated_bytes,
        }
    }

    fn writer(storage: &TemporaryStorage, run_id: u64, kind: RunKind) -> RunWriter {
        RunWriter::new(
            tempfile::tempfile().expect("temporary run file should open"),
            storage
                .reservation(0)
                .expect("empty reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), run_id, kind),
            512,
        )
        .expect("run writer should initialize")
    }

    fn reduce(
        storage: &TemporaryStorage,
        observations: &[IdentityObservation],
    ) -> Vec<(FileId, AllocationContribution)> {
        let mut input = writer(storage, 1, RunKind::IdentityObservation);
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in observations {
            append_identity_observation(&mut input, observation, &mut key, &mut value)
                .expect("fixture observations should be identity sorted");
        }
        let mut input = input
            .seal()
            .expect("input should seal")
            .into_reader()
            .expect("input should open");
        let mut output = writer(storage, 2, RunKind::AllocationContribution);
        reduce_identity_observations(&mut input, &mut output).expect("reduction should succeed");
        let mut output = output
            .seal()
            .expect("output should seal")
            .into_reader()
            .expect("output should open");
        let mut contributions = Vec::new();
        visit_allocation_contributions(&mut output, |file_id, contribution| {
            contributions.push((file_id, contribution));
            Ok(())
        })
        .expect("contributions should decode");
        contributions
    }

    #[test]
    fn identity_observation_codec_round_trips_file_identity_and_native_path() {
        let observation = observation(
            "folder/entry",
            FileId::new_high_res(7, 9),
            Some(1),
            ByteBounds::exact(42),
        );
        let mut key = Vec::new();
        let mut value = Vec::new();
        encode_identity_observation_into(&observation, &mut key, &mut value)
            .expect("observation should encode");
        assert_eq!(
            decode_identity_observation(&key, &value).expect("observation should decode"),
            observation
        );
    }

    #[test]
    fn singleton_identity_places_exact_allocation_at_its_leaf() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let file_id = FileId::new_inode(1, 2);
        let contributions = reduce(
            &storage,
            &[observation("entry", file_id, Some(1), ByteBounds::exact(8))],
        );
        assert_eq!(
            contributions,
            vec![(
                file_id,
                AllocationContribution {
                    recipient: path("entry"),
                    placement: AllocationPlacement::Leaf,
                    allocated_bytes: ByteBounds::exact(8),
                    reclaimable_bytes: ByteBounds::exact(8),
                },
            ),]
        );
    }

    #[test]
    fn linked_identity_places_one_shared_allocation_at_nested_common_ancestor() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let file_id = FileId::new_inode(1, 2);
        let contributions = reduce(
            &storage,
            &[
                observation("alpha/first", file_id, Some(2), ByteBounds::exact(8)),
                observation(
                    "alpha/nested/second",
                    file_id,
                    Some(2),
                    ByteBounds::exact(8),
                ),
            ],
        );
        assert_eq!(
            contributions,
            vec![(
                file_id,
                AllocationContribution {
                    recipient: path("alpha"),
                    placement: AllocationPlacement::Shared,
                    allocated_bytes: ByteBounds::exact(8),
                    reclaimable_bytes: ByteBounds::exact(8),
                },
            ),]
        );
    }

    #[test]
    fn inconsistent_identity_observations_stay_honestly_unknown() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let file_id = FileId::new_inode(1, 2);
        let contributions = reduce(
            &storage,
            &[
                observation("first", file_id, Some(3), ByteBounds::exact(8)),
                observation("second", file_id, Some(2), ByteBounds::exact(16)),
            ],
        );
        assert_eq!(contributions[0].1.placement, AllocationPlacement::Shared);
        assert_eq!(contributions[0].1.recipient, RelativePath::root());
        assert_eq!(contributions[0].1.allocated_bytes, ByteBounds::unknown());
        assert_eq!(contributions[0].1.reclaimable_bytes, ByteBounds::unknown());
    }

    #[test]
    fn identity_reduction_rejects_wrong_run_kind() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut input = writer(&storage, 1, RunKind::PathObservation)
            .seal()
            .expect("input should seal")
            .into_reader()
            .expect("input should open");
        let mut output = writer(&storage, 2, RunKind::AllocationContribution);
        assert!(matches!(
            reduce_identity_observations(&mut input, &mut output),
            Err(IdentityReductionError::WrongInputKind)
        ));
    }
}
