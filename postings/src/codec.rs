// SPDX-License-Identifier: AGPL-3.0-or-later

//! Experimental, canonical scalar encoding of a finite CTID set.
//!
//! Version 1 follows `docs/postings-format.md`. This is not a PostgreSQL storage
//! integration or a promise of durable-format compatibility. The checksum detects
//! accidental corruption; it does not authenticate a frame.

use std::fmt;
use std::mem::size_of;

use super::{Group, Page, Postings};

const MAGIC: [u8; 8] = *b"PLMBPST\0";
const HEADER_LEN: usize = 40;
const GROUP_HEADER_LEN: usize = 36;

/// Inclusive resource limits, checked before allocating decoded vector storage.
///
/// Structural storage counts requested `Group`, `Page`, and `u16` elements, not
/// allocator rounding, metadata, RSS, the borrowed frame, or the stack result.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    /// Maximum length of the complete encoded frame, including its header.
    pub max_encoded_bytes: usize,
    /// Maximum total number of tuple offsets.
    pub max_postings: usize,
    /// Maximum number of nonempty 256-block groups.
    pub max_groups: usize,
    /// Maximum number of nonempty pages across all groups.
    pub max_pages: usize,
    /// Maximum requested decoded vector element storage in bytes.
    pub max_decoded_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 64 * 1024 * 1024,
            max_postings: 8_000_000,
            max_groups: 65_536,
            max_pages: 1_000_000,
            max_decoded_bytes: 128 * 1024 * 1024,
        }
    }
}

/// A malformed frame, unsupported format, resource limit, or allocation failure.
///
/// Errors hold no allocated strings, including on the validation-only path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    /// The complete input exceeds the encoded-byte limit.
    EncodedBytesLimit,
    /// A required field or record is incomplete.
    Truncated,
    /// The format magic does not match.
    InvalidMagic,
    /// The format version is not supported.
    UnsupportedVersion,
    /// At least one unknown flag is set.
    UnsupportedFlags,
    /// The header length is not 40.
    InvalidHeaderLength,
    /// The reserved header field is nonzero.
    NonzeroReserved,
    /// The frame length disagrees with its body length, or body bytes remain.
    LengthMismatch,
    /// CRC32C verification failed.
    ChecksumMismatch,
    /// A size calculation overflowed or a field cannot represent a size.
    SizeOverflow,
    /// The number of groups exceeds the caller's limit.
    GroupsLimit,
    /// The number of pages exceeds the caller's limit.
    PagesLimit,
    /// The number of postings exceeds the caller's limit.
    PostingsLimit,
    /// Requested decoded element storage exceeds the caller's budget.
    DecodedBytesLimit,
    /// An aligned group base was required.
    UnalignedGroupBase,
    /// Group bases are not strictly increasing.
    GroupOrder,
    /// A group has an empty page mask.
    EmptyGroup,
    /// A page identifies the reserved block u32::MAX.
    InvalidBlock,
    /// A page has no offsets.
    EmptyPage,
    /// A tuple offset is zero.
    ZeroOffset,
    /// Tuple offsets are not strictly increasing.
    OffsetOrder,
    /// The header posting count disagrees with actual records.
    PostingCountMismatch,
    /// A fallible vector reservation failed.
    AllocationFailed,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EncodedBytesLimit => "encoded frame exceeds the byte limit",
            Self::Truncated => "encoded frame is truncated",
            Self::InvalidMagic => "invalid postings frame magic",
            Self::UnsupportedVersion => "unsupported postings format version",
            Self::UnsupportedFlags => "unsupported postings format flags",
            Self::InvalidHeaderLength => "postings header length must be 40",
            Self::NonzeroReserved => "reserved header field must be zero",
            Self::LengthMismatch => "frame or body length disagrees with its records",
            Self::ChecksumMismatch => "postings CRC32C checksum mismatch",
            Self::SizeOverflow => "postings size overflow or unrepresentable field",
            Self::GroupsLimit => "group count exceeds the limit",
            Self::PagesLimit => "page count exceeds the limit",
            Self::PostingsLimit => "posting count exceeds the limit",
            Self::DecodedBytesLimit => "decoded structural storage exceeds the limit",
            Self::UnalignedGroupBase => "group base must be divisible by 256",
            Self::GroupOrder => "group bases must be strictly increasing",
            Self::EmptyGroup => "group page mask must be nonzero",
            Self::InvalidBlock => "page block must not be u32::MAX",
            Self::EmptyPage => "page offset count must be nonzero",
            Self::ZeroOffset => "tuple offsets must be nonzero",
            Self::OffsetOrder => "tuple offsets must be strictly increasing",
            Self::PostingCountMismatch => "actual posting count disagrees with header",
            Self::AllocationFailed => "postings vector allocation failed",
        })
    }
}

impl std::error::Error for CodecError {}

fn add(a: usize, b: usize) -> Result<usize, CodecError> {
    a.checked_add(b).ok_or(CodecError::SizeOverflow)
}

fn multiply(a: usize, b: usize) -> Result<usize, CodecError> {
    a.checked_mul(b).ok_or(CodecError::SizeOverflow)
}

fn structural_bytes(groups: usize, pages: usize, postings: usize) -> Result<usize, CodecError> {
    add(
        add(
            multiply(groups, size_of::<Group>())?,
            multiply(pages, size_of::<Page>())?,
        )?,
        multiply(postings, size_of::<u16>())?,
    )
}

fn exact_vector<T>(count: usize) -> Result<Vec<T>, CodecError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| CodecError::AllocationFailed)?;
    Ok(result)
}

/// Encode an already validated set into its canonical version-1 frame.
///
/// Sizes and wire-field representability are checked before the sole allocation.
/// No intermediate flattened CTID collection is constructed.
pub fn encode(postings: &Postings) -> Result<Vec<u8>, CodecError> {
    let group_count = u32::try_from(postings.groups.len()).map_err(|_| CodecError::SizeOverflow)?;
    let posting_count = u64::try_from(postings.len).map_err(|_| CodecError::SizeOverflow)?;
    let mut body_len = multiply(postings.groups.len(), GROUP_HEADER_LEN)?;
    for group in &postings.groups {
        for page in &group.pages {
            u16::try_from(page.offsets.len()).map_err(|_| CodecError::SizeOverflow)?;
            body_len = add(body_len, add(2, multiply(page.offsets.len(), 2)?)?)?;
        }
    }
    let body_field = u32::try_from(body_len).map_err(|_| CodecError::SizeOverflow)?;
    let total_len = add(HEADER_LEN, body_len)?;
    let mut bytes = exact_vector(total_len)?;
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&group_count.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&posting_count.to_le_bytes());
    bytes.extend_from_slice(&body_field.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for group in &postings.groups {
        bytes.extend_from_slice(&group.base.to_le_bytes());
        for word in group.mask {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for page in &group.pages {
            let count = u16::try_from(page.offsets.len()).map_err(|_| CodecError::SizeOverflow)?;
            bytes.extend_from_slice(&count.to_le_bytes());
            for offset in &page.offsets {
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
        }
    }
    // The precomputed size covers every append, so none can grow the reservation.
    debug_assert_eq!(bytes.len(), total_len);
    let checksum = frame_checksum(&bytes)?;
    bytes
        .get_mut(36..HEADER_LEN)
        .ok_or(CodecError::Truncated)?
        .copy_from_slice(&checksum.to_le_bytes());
    Ok(bytes)
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let field = self.remaining.get(..N).ok_or(CodecError::Truncated)?;
        let value = field.try_into().map_err(|_| CodecError::Truncated)?;
        self.remaining = self.remaining.get(N..).ok_or(CodecError::Truncated)?;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, CodecError> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        self.take().map(u64::from_le_bytes)
    }

    fn group(&mut self) -> Result<(u32, [u64; 4]), CodecError> {
        Ok((
            self.u32()?,
            [self.u64()?, self.u64()?, self.u64()?, self.u64()?],
        ))
    }
}

fn page_indices(mask: [u64; 4]) -> impl Iterator<Item = u8> {
    // The index range makes array indexing and bit shifts bounded by construction.
    (0..=u8::MAX).filter(move |&index| mask[usize::from(index) / 64] & (1u64 << (index % 64)) != 0)
}

fn page_count(mask: [u64; 4]) -> Result<usize, CodecError> {
    // Four words have at most 256 set bits, so the u32 sum cannot overflow.
    usize::try_from(mask.iter().map(|word| word.count_ones()).sum::<u32>())
        .map_err(|_| CodecError::SizeOverflow)
}

struct Validated<'a> {
    body: &'a [u8],
    groups: usize,
    postings: usize,
}

fn validate(bytes: &[u8], limits: DecodeLimits) -> Result<Validated<'_>, CodecError> {
    // This must precede even header parsing and checksum work.
    if bytes.len() > limits.max_encoded_bytes {
        return Err(CodecError::EncodedBytesLimit);
    }
    let mut header = Reader { remaining: bytes };
    if header.take::<8>()? != MAGIC {
        return Err(CodecError::InvalidMagic);
    }
    if header.u16()? != 1 {
        return Err(CodecError::UnsupportedVersion);
    }
    if header.u16()? != 0 {
        return Err(CodecError::UnsupportedFlags);
    }
    if header.u32()? != 40 {
        return Err(CodecError::InvalidHeaderLength);
    }
    let groups = usize::try_from(header.u32()?).map_err(|_| CodecError::SizeOverflow)?;
    if header.u32()? != 0 {
        return Err(CodecError::NonzeroReserved);
    }
    let postings = usize::try_from(header.u64()?).map_err(|_| CodecError::SizeOverflow)?;
    let body_len = usize::try_from(header.u32()?).map_err(|_| CodecError::SizeOverflow)?;
    let checksum = header.u32()?;
    if add(HEADER_LEN, body_len)? != bytes.len() {
        return Err(CodecError::LengthMismatch);
    }
    if groups > limits.max_groups {
        return Err(CodecError::GroupsLimit);
    }
    if postings > limits.max_postings {
        return Err(CodecError::PostingsLimit);
    }
    if frame_checksum(bytes)? != checksum {
        return Err(CodecError::ChecksumMismatch);
    }
    let body = header.remaining;
    let mut reader = Reader { remaining: body };
    let mut previous_base = None;
    let mut actual_postings = 0;
    let mut pages = 0;
    if structural_bytes(groups, pages, actual_postings)? > limits.max_decoded_bytes {
        return Err(CodecError::DecodedBytesLimit);
    }
    for _ in 0..groups {
        let (base, mask) = reader.group()?;
        if base & 255 != 0 {
            return Err(CodecError::UnalignedGroupBase);
        }
        if previous_base.is_some_and(|previous| base <= previous) {
            return Err(CodecError::GroupOrder);
        }
        previous_base = Some(base);
        let group_pages = page_count(mask)?;
        if group_pages == 0 {
            return Err(CodecError::EmptyGroup);
        }
        pages = add(pages, group_pages)?;
        if pages > limits.max_pages {
            return Err(CodecError::PagesLimit);
        }
        for index in page_indices(mask) {
            let block = base
                .checked_add(u32::from(index))
                .ok_or(CodecError::InvalidBlock)?;
            if block == u32::MAX {
                return Err(CodecError::InvalidBlock);
            }
            let count = usize::from(reader.u16()?);
            if count == 0 {
                return Err(CodecError::EmptyPage);
            }
            actual_postings = add(actual_postings, count)?;
            if actual_postings > limits.max_postings {
                return Err(CodecError::PostingsLimit);
            }
            if actual_postings > postings {
                return Err(CodecError::PostingCountMismatch);
            }
            if structural_bytes(groups, pages, actual_postings)? > limits.max_decoded_bytes {
                return Err(CodecError::DecodedBytesLimit);
            }
            let mut previous = 0;
            for _ in 0..count {
                let offset = reader.u16()?;
                if offset == 0 {
                    return Err(CodecError::ZeroOffset);
                }
                if offset <= previous {
                    return Err(CodecError::OffsetOrder);
                }
                previous = offset;
            }
        }
    }
    if !reader.remaining.is_empty() {
        return Err(CodecError::LengthMismatch);
    }
    if actual_postings != postings {
        return Err(CodecError::PostingCountMismatch);
    }
    Ok(Validated {
        body,
        groups,
        postings,
    })
}

/// Validate an entire frame without allocation, then construct its grouped set.
///
/// The second pass makes only exact, fallible vector reservations, directly for
/// the validated groups, pages and offsets. Neither pass sorts or flattens CTIDs.
pub fn decode(bytes: &[u8], limits: DecodeLimits) -> Result<Postings, CodecError> {
    let validated = validate(bytes, limits)?;
    let mut reader = Reader {
        remaining: validated.body,
    };
    let mut groups = exact_vector(validated.groups)?;
    for _ in 0..validated.groups {
        let (base, mask) = reader.group()?;
        let mut pages = exact_vector(page_count(mask)?)?;
        for index in page_indices(mask) {
            let count = usize::from(reader.u16()?);
            let mut offsets = exact_vector(count)?;
            for _ in 0..count {
                offsets.push(reader.u16()?);
            }
            pages.push(Page { index, offsets });
        }
        groups.push(Group { base, mask, pages });
    }
    Ok(Postings {
        groups,
        len: validated.postings,
    })
}

fn crc32c<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    let mut crc = crate::accel::Crc32c::new();
    for part in parts {
        crc.update(part);
    }
    crc.finish()
}

fn frame_checksum(bytes: &[u8]) -> Result<u32, CodecError> {
    Ok(crc32c([
        bytes.get(..36).ok_or(CodecError::Truncated)?,
        bytes.get(HEADER_LEN..).ok_or(CodecError::Truncated)?,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ctid;

    #[test]
    fn castagnoli_check_vector() {
        assert_eq!(crc32c([b"123456789".as_slice()]), 0xe306_9283);
        assert_eq!(
            crc32c([b"1234".as_slice(), b"56789".as_slice()]),
            0xe306_9283
        );
    }

    #[test]
    fn structural_budget_is_exact_and_empty_costs_zero() {
        let postings = Postings::from_ctids([
            Ctid::new(0, 1).unwrap(),
            Ctid::new(0, 2).unwrap(),
            Ctid::new(255, 1).unwrap(),
            Ctid::new(256, 1).unwrap(),
        ]);
        let bytes = encode(&postings).unwrap();
        let budget = 2 * size_of::<Group>() + 3 * size_of::<Page>() + 4 * size_of::<u16>();
        let mut limits = DecodeLimits {
            max_decoded_bytes: budget,
            ..DecodeLimits::default()
        };
        assert_eq!(decode(&bytes, limits).unwrap(), postings);
        limits.max_decoded_bytes -= 1;
        assert_eq!(decode(&bytes, limits), Err(CodecError::DecodedBytesLimit));
        limits.max_decoded_bytes = 0;
        let empty = Postings::new();
        assert_eq!(decode(&encode(&empty).unwrap(), limits).unwrap(), empty);
    }

    #[test]
    fn structural_arithmetic_is_checked() {
        assert_eq!(
            structural_bytes(usize::MAX, 0, 0),
            Err(CodecError::SizeOverflow)
        );
        assert_eq!(
            structural_bytes(0, usize::MAX, 0),
            Err(CodecError::SizeOverflow)
        );
        assert_eq!(
            structural_bytes(0, 0, usize::MAX),
            Err(CodecError::SizeOverflow)
        );
    }
}
