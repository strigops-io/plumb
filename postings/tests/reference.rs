// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeSet;

use plumb_postings::{Ctid, CtidError, Postings};

fn ctid(block: u32, offset: u16) -> Ctid {
    Ctid::new(block, offset).unwrap()
}

fn assert_matches(actual: &Postings, expected: &BTreeSet<Ctid>) {
    let values: Vec<_> = actual.iter().collect();
    assert_eq!(values, expected.iter().copied().collect::<Vec<_>>());
    assert_eq!(actual.len(), expected.len());
    assert_eq!(actual.is_empty(), expected.is_empty());
    assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(values.iter().all(|&value| actual.contains(value)));
    for block in [0, 1, 63, 64, 255, 256, u32::MAX - 256, u32::MAX - 1] {
        for offset in [1, 2, 3, 17, 255, 256, u16::MAX] {
            let value = ctid(block, offset);
            assert_eq!(actual.contains(value), expected.contains(&value));
        }
    }
    // Canonical representation survives every operation, including deletion
    // of the last offset on a page or the last page in a group.
    assert_eq!(*actual, Postings::from_ctids(values));
}

#[test]
fn validates_ctids_and_exposes_components() {
    assert_eq!(Ctid::new(u32::MAX, 1), Err(CtidError::InvalidBlock));
    assert_eq!(Ctid::new(u32::MAX, u16::MAX), Err(CtidError::InvalidBlock));
    assert_eq!(Ctid::new(0, 0), Err(CtidError::ZeroOffset));
    assert_eq!(Ctid::new(u32::MAX - 1, 0), Err(CtidError::ZeroOffset));
    assert_eq!(Ctid::new(u32::MAX, 0), Err(CtidError::InvalidBlock));
    for block in [0, 63, 64, 255, 256, u32::MAX - 1] {
        for offset in [1, 2, 255, 256, 32768, u16::MAX] {
            let value = Ctid::new(block, offset).unwrap();
            assert_eq!(value.block(), block);
            assert_eq!(value.offset(), offset);
        }
    }
    assert_eq!(
        CtidError::InvalidBlock.to_string(),
        "CTID block must not be u32::MAX"
    );
    assert_eq!(
        CtidError::ZeroOffset.to_string(),
        "CTID offset must be nonzero"
    );
    fn is_error(_: &dyn std::error::Error) {}
    is_error(&CtidError::ZeroOffset);
    assert!(ctid(0, u16::MAX) < ctid(1, 1));
}

#[test]
fn bulk_build_orders_deduplicates_and_handles_empty_sets() {
    let values = [
        ctid(256, 9),
        ctid(0, u16::MAX),
        ctid(255, 1),
        ctid(64, 2),
        ctid(63, 3),
        ctid(64, 1),
        ctid(256, 9),
        ctid(u32::MAX - 1, u16::MAX),
        ctid(0, 1),
        ctid(64, 2),
    ];
    let actual = Postings::from_ctids(values);
    let expected = values.into_iter().collect();
    assert_matches(&actual, &expected);
    assert_eq!(actual, values.into_iter().rev().collect());
    assert_eq!(actual, Postings::from_ctids(actual.iter()));
    let empty = Postings::new();
    assert_matches(&empty, &BTreeSet::new());
    assert_eq!(empty, Postings::default());
    assert_eq!(empty, std::iter::empty::<Ctid>().collect());
    assert_eq!(empty.union(&empty), empty);
    assert_eq!(empty.intersection(&empty), empty);
    assert_eq!(empty.difference(&empty), empty);
    assert_eq!(actual.union(&empty), actual);
    assert_eq!(empty.union(&actual), actual);
    assert_eq!(actual.intersection(&empty), empty);
    assert_eq!(empty.intersection(&actual), empty);
    assert_eq!(actual.difference(&empty), actual);
    assert_eq!(empty.difference(&actual), empty);
}

#[test]
fn subtraction_preserves_shared_page_survivors_and_drops_empty_pages() {
    let left: Postings = [
        ctid(63, 1),
        ctid(63, 2),
        ctid(64, 7),
        ctid(255, 1),
        ctid(256, 1),
        ctid(u32::MAX - 1, u16::MAX),
    ]
    .into_iter()
    .collect();
    let right: Postings = [
        ctid(63, 2),
        ctid(63, 3),
        ctid(64, 7),
        ctid(256, 1),
        ctid(u32::MAX - 1, u16::MAX),
    ]
    .into_iter()
    .collect();
    assert_matches(
        &left.difference(&right),
        &[ctid(63, 1), ctid(255, 1)].into_iter().collect(),
    );
    assert_matches(
        &right.difference(&left),
        &[ctid(63, 3)].into_iter().collect(),
    );
    assert_eq!(left.difference(&left), Postings::new());
}

#[test]
fn every_page_mask_bit_and_adjacent_groups() {
    let left: Postings = (0..768).map(|block| ctid(block, 1)).collect();
    let right: Postings = (0..768)
        .filter(|block| block % 3 != 0)
        .flat_map(|block| [ctid(block, 1), ctid(block, 2)])
        .collect();
    let a: BTreeSet<_> = left.iter().collect();
    let b: BTreeSet<_> = right.iter().collect();
    assert_matches(&left.union(&right), &a.union(&b).copied().collect());
    assert_matches(
        &left.intersection(&right),
        &a.intersection(&b).copied().collect(),
    );
    assert_matches(
        &left.difference(&right),
        &a.difference(&b).copied().collect(),
    );
}

#[test]
fn all_nonzero_u16_offsets_are_supported() {
    let all: Postings = (1..=u16::MAX)
        .map(|offset| ctid(u32::MAX - 1, offset))
        .collect();
    let odds: Postings = (1..=u16::MAX)
        .filter(|offset| offset % 2 == 1)
        .map(|offset| ctid(u32::MAX - 1, offset))
        .collect();
    assert_eq!(all.len(), usize::from(u16::MAX));
    let a: BTreeSet<_> = all.iter().collect();
    let b: BTreeSet<_> = odds.iter().collect();
    assert_matches(&all.difference(&odds), &a.difference(&b).copied().collect());
    assert_eq!(all.union(&odds), all);
    assert_eq!(all.intersection(&odds), odds);
}

// Deterministic, dependency-free generator: reproducibility matters more than
// statistical quality. Wrapping arithmetic is explicit in debug and release.
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }

    fn ctids(&mut self, count: usize) -> Vec<Ctid> {
        let blocks = [
            0,
            1,
            62,
            63,
            64,
            65,
            127,
            128,
            191,
            192,
            254,
            255,
            256,
            257,
            511,
            512,
            65535,
            65536,
            1 << 31,
            u32::MAX - 257,
            u32::MAX - 256,
            u32::MAX - 255,
            u32::MAX - 2,
            u32::MAX - 1,
        ];
        let mut result = Vec::new();
        for i in 0..count {
            let block = if i % 7 == 0 {
                self.next() % u32::MAX
            } else {
                blocks[self.next() as usize % blocks.len()]
            };
            let offset = match self.next() % 7 {
                0 => 1,
                1 => 2,
                2 => u16::MAX,
                _ => (self.next() % 17 + 1) as u16,
            };
            let value = ctid(block, offset);
            result.push(value);
            if i % 3 == 0 {
                result.push(value);
            }
        }
        result.reverse();
        result
    }
}

#[test]
fn generated_reference_sets_and_algebra_identities() {
    for seed in 0..96 {
        let mut generator = Generator(seed);
        let a = generator.ctids(if seed % 13 == 0 { 0 } else { 180 });
        let b = generator.ctids(if seed % 11 == 0 { 0 } else { 220 });
        let c = generator.ctids(100);
        let ar: BTreeSet<_> = a.iter().copied().collect();
        let br: BTreeSet<_> = b.iter().copied().collect();
        let a = Postings::from_ctids(a);
        let b = Postings::from_ctids(b);
        let c = Postings::from_ctids(c);
        assert_matches(&a, &ar);
        assert_matches(&b, &br);
        assert_matches(&a.union(&b), &ar.union(&br).copied().collect());
        assert_matches(
            &a.intersection(&b),
            &ar.intersection(&br).copied().collect(),
        );
        assert_matches(&a.difference(&b), &ar.difference(&br).copied().collect());
        assert_matches(&b.difference(&a), &br.difference(&ar).copied().collect());
        assert_eq!(a.union(&b), b.union(&a), "union commutativity: {seed}");
        assert_eq!(
            a.intersection(&b),
            b.intersection(&a),
            "intersection commutativity: {seed}"
        );
        assert_eq!(a.union(&a), a);
        assert_eq!(a.intersection(&a), a);
        assert_eq!(a.difference(&a), Postings::new());
        assert_eq!(a.union(&b).union(&c), a.union(&b.union(&c)));
        assert_eq!(
            a.intersection(&b).intersection(&c),
            a.intersection(&b.intersection(&c))
        );
        assert_eq!(
            a.intersection(&b.union(&c)),
            a.intersection(&b).union(&a.intersection(&c))
        );
        assert_eq!(
            a.union(&b.intersection(&c)),
            a.union(&b).intersection(&a.union(&c))
        );
        assert_eq!(a.union(&a.intersection(&b)), a);
        assert_eq!(a.intersection(&a.union(&b)), a);
        assert_eq!(a.difference(&b).union(&a.intersection(&b)), a);
        assert!(a.difference(&b).intersection(&b).is_empty());
        assert_eq!(a.difference(&b.union(&c)), a.difference(&b).difference(&c));
    }
}

#[test]
fn exhaustive_small_universe_reference_pairs() {
    let universe = [
        ctid(63, 1),
        ctid(63, 2),
        ctid(64, 1),
        ctid(255, u16::MAX),
        ctid(256, 1),
        ctid(u32::MAX - 1, 1),
    ];
    let subsets: Vec<(Postings, BTreeSet<Ctid>)> = (0..(1 << universe.len()))
        .map(|bits| {
            let reference: BTreeSet<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| bits & (1 << i) != 0)
                .map(|(_, &value)| value)
                .collect();
            (reference.iter().copied().collect(), reference)
        })
        .collect();
    for (a, ar) in &subsets {
        for (b, br) in &subsets {
            assert_matches(&a.union(b), &ar.union(br).copied().collect());
            assert_matches(&a.intersection(b), &ar.intersection(br).copied().collect());
            assert_matches(&a.difference(b), &ar.difference(br).copied().collect());
        }
    }
}
