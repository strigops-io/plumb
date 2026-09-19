// SPDX-License-Identifier: AGPL-3.0-or-later

//! Safe, scalar, in-memory CTID postings.
//!
//! This is a reference implementation of set semantics, not a persistent format
//! or a PostgreSQL integration. Blocks are grouped in aligned ranges of 256;
//! each group has a four-word page mask and sorted, unique sparse offsets for
//! each present page. Private representation and validated [`Ctid`] values keep
//! empty pages, duplicate offsets, and invalid CTIDs out of every public result.
//!
//! ```
//! use plumb_postings::{Ctid, Postings};
//!
//! let first = Ctid::new(255, 1)?;
//! let next = Ctid::new(256, 2)?;
//! let left = Postings::from_ctids([next, first, first]);
//! let right = Postings::from_ctids([next]);
//! assert_eq!(left.len(), 2);
//! assert_eq!(left.iter().collect::<Vec<_>>(), vec![first, next]);
//! assert_eq!(left.difference(&right).iter().collect::<Vec<_>>(), vec![first]);
//! # Ok::<(), plumb_postings::CtidError>(())
//! ```

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::fmt;

/// A validated physical tuple identifier, ordered by block then offset.
///
/// All blocks except `u32::MAX` are accepted. All nonzero `u16` offsets are
/// accepted: a PostgreSQL-specific physical page limit belongs at integration
/// boundaries, not in this scalar set representation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Ctid {
    block: u32,
    offset: u16,
}

impl Ctid {
    /// Validate a block and offset. If both are invalid, the block error wins.
    pub const fn new(block: u32, offset: u16) -> Result<Self, CtidError> {
        if block == u32::MAX {
            Err(CtidError::InvalidBlock)
        } else if offset == 0 {
            Err(CtidError::ZeroOffset)
        } else {
            Ok(Self { block, offset })
        }
    }

    /// Return the heap block number.
    pub const fn block(self) -> u32 {
        self.block
    }

    /// Return the nonzero tuple offset.
    pub const fn offset(self) -> u16 {
        self.offset
    }
}

/// Validation failure when constructing a [`Ctid`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtidError {
    /// `u32::MAX` is reserved and cannot identify a heap block.
    InvalidBlock,
    /// Tuple offsets are one-based; zero is invalid.
    ZeroOffset,
}

impl fmt::Display for CtidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidBlock => "CTID block must not be u32::MAX",
            Self::ZeroOffset => "CTID offset must be nonzero",
        })
    }
}

impl std::error::Error for CtidError {}

/// An immutable set of CTIDs with ascending, duplicate-free iteration.
///
/// Bulk construction sorts and deduplicates its input. Set operations merge
/// sorted groups, use page masks to gate page work, and merge sorted offsets.
/// No tree set or flattened CTID collection is used by the set operations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Postings {
    groups: Vec<Group>,
    len: usize,
}

// Groups are nonempty and sorted by aligned base; pages are nonempty and sorted
// by index. A mask bit is set if and only if the corresponding page is present.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Group {
    base: u32,
    mask: [u64; 4],
    pages: Vec<Page>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Page {
    index: u8,
    offsets: Vec<u16>,
}

impl Group {
    fn new(base: u32) -> Self {
        Self {
            base,
            mask: [0; 4],
            pages: Vec::new(),
        }
    }

    fn push_page(&mut self, index: u8, offsets: Vec<u16>) {
        debug_assert!(!offsets.is_empty());
        debug_assert!(self.pages.last().is_none_or(|page| page.index < index));
        self.mask[index as usize / 64] |= 1u64 << (index % 64);
        self.pages.push(Page { index, offsets });
    }

    // The mask rejects absent pages before any offset lookup or merge.
    fn offsets(&self, index: u8) -> &[u16] {
        if self.mask[index as usize / 64] & (1u64 << (index % 64)) == 0 {
            return &[];
        }
        let position = self
            .pages
            .binary_search_by_key(&index, |page| page.index)
            .expect("a set mask bit always has a page");
        &self.pages[position].offsets
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Union,
    Intersection,
    Difference,
}

impl Operation {
    fn keep_left(self) -> bool {
        matches!(self, Self::Union | Self::Difference)
    }

    fn keep_right(self) -> bool {
        matches!(self, Self::Union)
    }

    fn keep_equal(self) -> bool {
        matches!(self, Self::Union | Self::Intersection)
    }
}

impl Postings {
    /// Construct an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bulk-build a set, sorting and deduplicating arbitrary input order.
    pub fn from_ctids(ctids: impl IntoIterator<Item = Ctid>) -> Self {
        let mut ctids: Vec<_> = ctids.into_iter().collect();
        ctids.sort_unstable();
        ctids.dedup();
        let mut result = Self {
            groups: Vec::new(),
            len: ctids.len(),
        };
        for ctid in ctids {
            let base = ctid.block & !255;
            let index = (ctid.block & 255) as u8;
            if result.groups.last().is_none_or(|group| group.base != base) {
                result.groups.push(Group::new(base));
            }
            let group = result.groups.last_mut().expect("group was just ensured");
            if group.pages.last().is_none_or(|page| page.index != index) {
                group.push_page(index, vec![ctid.offset]);
            } else {
                group
                    .pages
                    .last_mut()
                    .expect("page exists")
                    .offsets
                    .push(ctid.offset);
            }
        }
        result
    }

    /// Number of distinct CTIDs (not pages or groups).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the set has no CTIDs.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterate by ascending block, then ascending offset, without duplicates.
    pub fn iter(&self) -> impl Iterator<Item = Ctid> + '_ {
        self.groups.iter().flat_map(|group| {
            group.pages.iter().flat_map(move |page| {
                page.offsets.iter().map(move |&offset| Ctid {
                    block: group.base | u32::from(page.index),
                    offset,
                })
            })
        })
    }

    /// Test membership, using the page mask before looking up offsets.
    pub fn contains(&self, ctid: Ctid) -> bool {
        let base = ctid.block & !255;
        self.groups
            .binary_search_by_key(&base, |group| group.base)
            .is_ok_and(|i| {
                self.groups[i]
                    .offsets((ctid.block & 255) as u8)
                    .binary_search(&ctid.offset)
                    .is_ok()
            })
    }

    /// All CTIDs present in either operand.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        self.merge(other, Operation::Union)
    }

    /// All CTIDs present in both operands.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        self.merge(other, Operation::Intersection)
    }

    /// Explicit set subtraction: CTIDs in `self` but not in `other`.
    ///
    /// This is not SQL/TINQL `NOT`, a universe complement, or a statement about
    /// NULL values or visibility. The operands are plain finite CTID sets.
    #[must_use]
    pub fn difference(&self, other: &Self) -> Self {
        self.merge(other, Operation::Difference)
    }

    fn merge(&self, other: &Self, operation: Operation) -> Self {
        let mut result = Self::new();
        let (mut left, mut right) = (0, 0);
        while left < self.groups.len() && right < other.groups.len() {
            let a = &self.groups[left];
            let b = &other.groups[right];
            match a.base.cmp(&b.base) {
                Ordering::Less => {
                    if operation.keep_left() {
                        result.groups.push(a.clone());
                    }
                    left += 1;
                }
                Ordering::Greater => {
                    if operation.keep_right() {
                        result.groups.push(b.clone());
                    }
                    right += 1;
                }
                Ordering::Equal => {
                    let group = merge_group(a, b, operation);
                    if !group.pages.is_empty() {
                        result.groups.push(group);
                    }
                    left += 1;
                    right += 1;
                }
            }
        }
        if operation.keep_left() {
            result.groups.extend_from_slice(&self.groups[left..]);
        }
        if operation.keep_right() {
            result.groups.extend_from_slice(&other.groups[right..]);
        }
        result.len = result
            .groups
            .iter()
            .flat_map(|group| &group.pages)
            .map(|page| page.offsets.len())
            .sum();
        result
    }
}

impl FromIterator<Ctid> for Postings {
    fn from_iter<T: IntoIterator<Item = Ctid>>(iter: T) -> Self {
        Self::from_ctids(iter)
    }
}

fn merge_group(left: &Group, right: &Group, operation: Operation) -> Group {
    debug_assert_eq!(left.base, right.base);
    let mut result = Group::new(left.base);
    for word in 0..4 {
        let mut candidates = match operation {
            Operation::Union => left.mask[word] | right.mask[word],
            Operation::Intersection => left.mask[word] & right.mask[word],
            // Shared pages still need offset subtraction; NOT of the right
            // page mask would incorrectly discard all their surviving offsets.
            Operation::Difference => left.mask[word],
        };
        while candidates != 0 {
            let index = (word * 64 + candidates.trailing_zeros() as usize) as u8;
            candidates &= candidates - 1;
            let offsets = merge_offsets(left.offsets(index), right.offsets(index), operation);
            if !offsets.is_empty() {
                result.push_page(index, offsets);
            }
        }
    }
    result
}

fn merge_offsets(left: &[u16], right: &[u16], operation: Operation) -> Vec<u16> {
    let mut result = Vec::new();
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        match left[a].cmp(&right[b]) {
            Ordering::Less => {
                if operation.keep_left() {
                    result.push(left[a]);
                }
                a += 1;
            }
            Ordering::Greater => {
                if operation.keep_right() {
                    result.push(right[b]);
                }
                b += 1;
            }
            Ordering::Equal => {
                if operation.keep_equal() {
                    result.push(left[a]);
                }
                a += 1;
                b += 1;
            }
        }
    }
    if operation.keep_left() {
        result.extend_from_slice(&left[a..]);
    }
    if operation.keep_right() {
        result.extend_from_slice(&right[b..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invariants(postings: &Postings) {
        assert!(
            postings
                .groups
                .windows(2)
                .all(|pair| pair[0].base < pair[1].base)
        );
        let mut count = 0;
        for group in &postings.groups {
            assert_eq!(group.base & 255, 0);
            assert!(!group.pages.is_empty());
            assert!(
                group
                    .pages
                    .windows(2)
                    .all(|pair| pair[0].index < pair[1].index)
            );
            let mut mask = [0u64; 4];
            for page in &group.pages {
                assert!(!page.offsets.is_empty());
                assert!(page.offsets.windows(2).all(|pair| pair[0] < pair[1]));
                assert!(page.offsets.iter().all(|&offset| offset != 0));
                assert_ne!(group.base | u32::from(page.index), u32::MAX);
                mask[page.index as usize / 64] |= 1u64 << (page.index % 64);
                count += page.offsets.len();
            }
            assert_eq!(group.mask, mask);
        }
        assert_eq!(count, postings.len);
        assert_eq!(postings.is_empty(), postings.groups.is_empty());
    }

    #[test]
    fn representation_masks_and_result_invariants() {
        let blocks = [0, 63, 64, 127, 128, 191, 192, 255, 256, u32::MAX - 1];
        let left =
            Postings::from_ctids(blocks.into_iter().flat_map(|block| {
                [1, 2, u16::MAX].map(|offset| Ctid::new(block, offset).unwrap())
            }));
        assert_eq!(left.groups[0].mask, [(1u64 << 63) | 1; 4]);
        assert_eq!(left.groups[1].mask, [1, 0, 0, 0]);
        assert_eq!(left.groups[2].mask, [0, 0, 0, 1u64 << 62]);
        let right =
            Postings::from_ctids(blocks.into_iter().flat_map(|block| {
                [2, 3, u16::MAX].map(|offset| Ctid::new(block, offset).unwrap())
            }));
        for result in [
            left.clone(),
            right.clone(),
            left.union(&right),
            left.intersection(&right),
            left.difference(&right),
            right.difference(&left),
            left.difference(&left),
            Postings::new(),
        ] {
            assert_invariants(&result);
        }
    }
}
