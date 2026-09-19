// SPDX-License-Identifier: AGPL-3.0-or-later

//! Independent wire-format oracle and hostile-input tests. No codec internals,
//! private layout sizes, third-party dependencies, or production CRC helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::catch_unwind;

use plumb_postings::codec::{CodecError, DecodeLimits, decode, encode};
use plumb_postings::{Ctid, Postings};

const HEADER: usize = 40;

fn ctid(block: u32, offset: u16) -> Ctid {
    Ctid::new(block, offset).unwrap()
}

// Deliberately simple bit-at-a-time reference CRC32C.
fn crc32c(bytes: impl IntoIterator<Item = u8>) -> u32 {
    let mut register = !0u32;
    for byte in bytes {
        register ^= u32::from(byte);
        for _ in 0..8 {
            register = (register >> 1) ^ if register & 1 != 0 { 0x82f6_3b78 } else { 0 };
        }
    }
    !register
}

fn checksum(frame: &[u8]) -> u32 {
    crc32c(frame[..36].iter().chain(&frame[HEADER..]).copied())
}

fn reseal(frame: &mut [u8]) {
    let crc = checksum(frame);
    frame[36..40].copy_from_slice(&crc.to_le_bytes());
}

fn assert_sealed(frame: &[u8]) {
    assert_eq!(&frame[36..40], &checksum(frame).to_le_bytes());
}

// Counts are deliberately caller-controlled: this also assembles malformed
// frames whose checksums are nevertheless valid.
fn frame(body: &[u8], groups: u32, postings: u64) -> Vec<u8> {
    let mut result = b"PLMBPST\0".to_vec();
    result.extend(1u16.to_le_bytes());
    result.extend(0u16.to_le_bytes());
    result.extend(40u32.to_le_bytes());
    result.extend(groups.to_le_bytes());
    result.extend(0u32.to_le_bytes());
    result.extend(postings.to_le_bytes());
    result.extend(u32::try_from(body.len()).unwrap().to_le_bytes());
    result.extend(0u32.to_le_bytes());
    result.extend(body);
    reseal(&mut result);
    result
}

fn group(base: u32, pages: &[(u8, Vec<u16>)]) -> Vec<u8> {
    let mut result = base.to_le_bytes().to_vec();
    let mut words = [0u64; 4];
    for (page, _) in pages {
        words[usize::from(*page) / 64] |= 1u64 << (*page % 64);
    }
    for word in words {
        result.extend(word.to_le_bytes());
    }
    for (_, offsets) in pages {
        result.extend(u16::try_from(offsets.len()).unwrap().to_le_bytes());
        for offset in offsets {
            result.extend(offset.to_le_bytes());
        }
    }
    result
}

fn reference_frame(values: &BTreeSet<Ctid>) -> Vec<u8> {
    let mut groups: BTreeMap<u32, BTreeMap<u8, Vec<u16>>> = BTreeMap::new();
    for value in values {
        groups
            .entry(value.block() & !255)
            .or_default()
            .entry((value.block() % 256) as u8)
            .or_default()
            .push(value.offset());
    }
    let mut body = Vec::new();
    for (&base, pages) in &groups {
        let pages: Vec<_> = pages
            .iter()
            .map(|(&page, offsets)| (page, offsets.clone()))
            .collect();
        body.extend(group(base, &pages));
    }
    frame(&body, groups.len() as u32, values.len() as u64)
}

fn assert_matches(actual: &Postings, expected: &BTreeSet<Ctid>) {
    assert_eq!(
        actual.iter().collect::<Vec<_>>(),
        expected.iter().copied().collect::<Vec<_>>()
    );
    assert_eq!(actual.len(), expected.len());
    assert_eq!(actual.is_empty(), expected.is_empty());
    for &value in expected {
        assert!(actual.contains(value));
    }
    assert_eq!(encode(actual).unwrap(), reference_frame(expected));
}

fn reject(label: &str, bytes: &[u8], limits: DecodeLimits) {
    let result = catch_unwind(|| decode(bytes, limits));
    assert!(result.is_ok(), "decoder panicked: {label}");
    assert!(result.unwrap().is_err(), "decoder accepted: {label}");
}

fn reject_sealed(label: &str, bytes: &[u8]) {
    // Ensures the following failure cannot be explained by an invalid CRC.
    assert_sealed(bytes);
    reject(label, bytes, DecodeLimits::default());
}

fn hex(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

fn boundary_set() -> BTreeSet<Ctid> {
    [ctid(255, 1), ctid(255, 65535), ctid(256, 256)]
        .into_iter()
        .collect()
}

#[test]
fn crc_check_value_and_literal_golden_frames() {
    assert_eq!(crc32c(*b"123456789"), 0xe306_9283);
    // Literal fixtures were independently computed using the non-reflected
    // Castagnoli polynomial 0x1edc6f41 with explicit bit reversal.
    let empty = hex("50 4c 4d 42 50 53 54 00 01 00 00 00 28 00 00 00
                     00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
                     00 00 00 00 2c 2d 5e c8");
    let boundary = hex("50 4c 4d 42 50 53 54 00 01 00 00 00 28 00 00 00
                        02 00 00 00 00 00 00 00 03 00 00 00 00 00 00 00
                        52 00 00 00 64 ea 2b 28
                        00 00 00 00
                        00 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 80
                        02 00 01 00 ff ff
                        00 01 00 00
                        01 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 00
                        00 00 00 00 00 00 00 00
                        01 00 00 01");
    assert_eq!(boundary.len(), 122);
    for (wire, values) in [(empty, BTreeSet::new()), (boundary, boundary_set())] {
        assert_sealed(&wire);
        assert_eq!(reference_frame(&values), wire);
        assert_eq!(
            encode(&Postings::from_ctids(values.iter().copied())).unwrap(),
            wire
        );
        assert_matches(&decode(&wire, DecodeLimits::default()).unwrap(), &values);
    }
}

struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }

    fn values(&mut self, count: usize) -> Vec<Ctid> {
        let edges = [
            0,
            1,
            63,
            64,
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
            u32::MAX - 256,
            u32::MAX - 1,
        ];
        (0..count)
            .map(|i| {
                let block = if i % 5 == 0 {
                    self.next() % u32::MAX
                } else {
                    edges[self.next() as usize % edges.len()]
                };
                let offset = match self.next() % 5 {
                    0 => 1,
                    1 => u16::MAX,
                    _ => (self.next() % 257 + 1) as u16,
                };
                ctid(block, offset)
            })
            .collect()
    }
}

#[test]
fn generated_roundtrips_are_canonical_regardless_of_order_and_duplicates() {
    for seed in 0..48 {
        let mut rng = Generator(seed);
        let input = rng.values(if seed % 11 == 0 { 0 } else { 120 });
        let expected: BTreeSet<_> = input.iter().copied().collect();
        let wire = reference_frame(&expected);
        let original = Postings::from_ctids(input.iter().copied());
        assert_eq!(encode(&original).unwrap(), wire, "seed {seed}");
        let mut shuffled = input.clone();
        for i in (1..shuffled.len()).rev() {
            let j = rng.next() as usize % (i + 1);
            shuffled.swap(i, j);
        }
        shuffled.extend(input.iter().copied().rev());
        shuffled.extend(input.iter().copied());
        assert_eq!(encode(&Postings::from_ctids(shuffled)).unwrap(), wire);
        let decoded = decode(&wire, DecodeLimits::default()).unwrap();
        assert_eq!(decoded, original);
        assert_matches(&decoded, &expected);
    }
}

#[test]
fn all_mask_bits_maximum_valid_block_and_full_u16_offset_count() {
    let values: BTreeSet<_> = (0..512)
        .map(|block| ctid(block, 1))
        .chain((1..=u16::MAX).map(|offset| ctid(u32::MAX - 1, offset)))
        .chain([ctid(u32::MAX - 255, 32768), ctid(u32::MAX - 256, 256)])
        .collect();
    let wire = reference_frame(&values);
    let decoded = decode(&wire, DecodeLimits::default()).unwrap();
    assert_matches(&decoded, &values);
    assert_eq!(decoded, Postings::from_ctids(values));
}

#[test]
fn every_truncation_and_appended_suffix_is_rejected() {
    for wire in [
        reference_frame(&BTreeSet::new()),
        reference_frame(&boundary_set()),
    ] {
        for end in 0..wire.len() {
            reject(
                &format!("truncated at {end}"),
                &wire[..end],
                DecodeLimits::default(),
            );
        }
        for suffix in [vec![0], vec![255], vec![0; 40], wire.clone()] {
            let mut appended = wire.clone();
            appended.extend(suffix);
            reject("trailing bytes", &appended, DecodeLimits::default());
            // Also reject extra body bytes when header length and CRC agree.
            let length = (appended.len() - HEADER) as u32;
            appended[32..36].copy_from_slice(&length.to_le_bytes());
            reseal(&mut appended);
            reject_sealed("trailing bytes included in declared body", &appended);
        }
    }
}

#[test]
fn every_single_bit_in_header_checksum_and_body_is_protected() {
    let wire = reference_frame(&boundary_set());
    for byte in 0..wire.len() {
        for bit in 0..8 {
            let mut corrupted = wire.clone();
            corrupted[byte] ^= 1 << bit;
            reject(
                &format!("bit {bit} of byte {byte}"),
                &corrupted,
                DecodeLimits::default(),
            );
        }
    }
    // Changing this offset from 1 to 3 preserves canonical structure. It must
    // fail with the old CRC and succeed with a fresh CRC: checksum validation
    // is exercised independently of all structural checks.
    let mut valid_change = wire;
    valid_change[78] ^= 2;
    reject(
        "structurally valid offset change with stale CRC",
        &valid_change,
        DecodeLimits::default(),
    );
    reseal(&mut valid_change);
    let expected = [ctid(255, 3), ctid(255, 65535), ctid(256, 256)]
        .into_iter()
        .collect();
    assert_matches(
        &decode(&valid_change, DecodeLimits::default()).unwrap(),
        &expected,
    );
}

#[test]
fn resealed_header_corruptions_fail_independently_of_crc() {
    let valid = reference_frame(&boundary_set());
    let replacements: Vec<(&str, usize, Vec<u8>)> = vec![
        ("magic", 0, vec![0]),
        ("magic terminator", 7, vec![1]),
        ("version zero", 8, 0u16.to_le_bytes().to_vec()),
        ("future version", 8, 2u16.to_le_bytes().to_vec()),
        ("flags", 10, 1u16.to_le_bytes().to_vec()),
        ("high flag", 10, 0x8000u16.to_le_bytes().to_vec()),
        ("short header", 12, 39u32.to_le_bytes().to_vec()),
        ("long header", 12, 41u32.to_le_bytes().to_vec()),
        ("huge header", 12, u32::MAX.to_le_bytes().to_vec()),
        ("zero group count", 16, 0u32.to_le_bytes().to_vec()),
        ("too few groups", 16, 1u32.to_le_bytes().to_vec()),
        ("too many groups", 16, 3u32.to_le_bytes().to_vec()),
        ("reserved", 20, 1u32.to_le_bytes().to_vec()),
        ("high reserved", 20, 0x80000000u32.to_le_bytes().to_vec()),
        ("zero postings", 24, 0u64.to_le_bytes().to_vec()),
        ("too few postings", 24, 2u64.to_le_bytes().to_vec()),
        ("too many postings", 24, 4u64.to_le_bytes().to_vec()),
        ("short body length", 32, 81u32.to_le_bytes().to_vec()),
        ("long body length", 32, 83u32.to_le_bytes().to_vec()),
        ("huge body length", 32, u32::MAX.to_le_bytes().to_vec()),
    ];
    for (label, start, bytes) in replacements {
        let mut bad = valid.clone();
        bad[start..start + bytes.len()].copy_from_slice(&bytes);
        reseal(&mut bad);
        reject_sealed(label, &bad);
    }
    reject_sealed("empty body with group", &frame(&[], 1, 0));
    reject_sealed("empty body with posting", &frame(&[], 0, 1));
    reject_sealed("empty body with group and posting", &frame(&[], 1, 1));
}

#[test]
fn resealed_noncanonical_groups_pages_and_offsets_are_rejected() {
    let cases = [
        ("zero offset", group(0, &[(0, vec![0])]), 1, 1),
        ("duplicate offsets", group(0, &[(0, vec![1, 1])]), 1, 2),
        ("descending offsets", group(0, &[(0, vec![2, 1])]), 1, 2),
        ("empty page", group(0, &[(0, vec![])]), 1, 1),
        ("empty group", group(0, &[]), 1, 1),
        ("unaligned group", group(1, &[(0, vec![1])]), 1, 1),
        ("reserved block", group(!255u32, &[(255, vec![1])]), 1, 1),
        (
            "reserved block following valid high page",
            group(!255u32, &[(254, vec![1]), (255, vec![2])]),
            1,
            2,
        ),
        (
            "duplicate groups",
            [group(0, &[(0, vec![1])]), group(0, &[(1, vec![2])])].concat(),
            2,
            2,
        ),
        (
            "descending groups",
            [group(256, &[(0, vec![1])]), group(0, &[(1, vec![2])])].concat(),
            2,
            2,
        ),
        (
            "empty group after valid group",
            [group(0, &[(0, vec![1])]), group(256, &[])].concat(),
            2,
            1,
        ),
        (
            "empty page following valid page",
            group(0, &[(0, vec![1]), (1, vec![])]),
            1,
            1,
        ),
    ];
    for (label, body, groups, postings) in cases {
        reject_sealed(label, &frame(&body, groups, postings));
    }
    let body = group(0, &[(0, vec![1, 2])]);
    // Rebuild lengths/checksum at each cut so CRC and declared frame length
    // cannot mask truncation within a base, mask, page count, or tuple offset.
    for end in 0..body.len() {
        reject_sealed(
            &format!("short group body {end}"),
            &frame(&body[..end], 1, 2),
        );
    }
    let mut bad_count = body.clone();
    bad_count[36..38].copy_from_slice(&u16::MAX.to_le_bytes());
    reject_sealed("huge page offset count", &frame(&bad_count, 1, 65535));
    let mut missing_page = body;
    missing_page[4] |= 2;
    reject_sealed(
        "mask requests missing page record",
        &frame(&missing_page, 1, 2),
    );
}

#[test]
fn defaults_traits_and_inclusive_count_and_encoded_limits() {
    fn require_traits<T: Default + Clone + Copy + std::fmt::Debug>() {}
    require_traits::<DecodeLimits>();
    fn require_error<T: std::error::Error>() {}
    require_error::<CodecError>();
    let defaults = DecodeLimits::default();
    assert_eq!(defaults.max_encoded_bytes, 64 * 1024 * 1024);
    assert_eq!(defaults.max_postings, 8_000_000);
    assert_eq!(defaults.max_groups, 65_536);
    assert_eq!(defaults.max_pages, 1_000_000);
    assert_eq!(defaults.max_decoded_bytes, 128 * 1024 * 1024);

    let values = [ctid(0, 1), ctid(0, 2), ctid(255, 3), ctid(256, 4)]
        .into_iter()
        .collect();
    let wire = reference_frame(&values);
    let exact = DecodeLimits {
        max_encoded_bytes: wire.len(),
        max_postings: 4,
        max_groups: 2,
        max_pages: 3,
        ..defaults
    };
    assert_matches(&decode(&wire, exact).unwrap(), &values);
    for limits in [
        DecodeLimits {
            max_encoded_bytes: wire.len() - 1,
            ..exact
        },
        DecodeLimits {
            max_postings: 3,
            ..exact
        },
        DecodeLimits {
            max_groups: 1,
            ..exact
        },
        DecodeLimits {
            max_pages: 2,
            ..exact
        },
        DecodeLimits {
            max_encoded_bytes: 0,
            ..exact
        },
        DecodeLimits {
            max_postings: 0,
            ..exact
        },
        DecodeLimits {
            max_groups: 0,
            ..exact
        },
        DecodeLimits {
            max_pages: 0,
            ..exact
        },
    ] {
        reject("one count budget below exact requirement", &wire, limits);
    }
    let zero = DecodeLimits {
        max_encoded_bytes: HEADER,
        max_postings: 0,
        max_groups: 0,
        max_pages: 0,
        max_decoded_bytes: 0,
    };
    let empty = reference_frame(&BTreeSet::new());
    assert!(decode(&empty, zero).unwrap().is_empty());
    reject(
        "empty header still costs encoded bytes",
        &empty,
        DecodeLimits {
            max_encoded_bytes: HEADER - 1,
            ..zero
        },
    );
    let error = decode(&wire, zero).unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(!format!("{error:?}").is_empty());
}

fn minimum_storage(wire: &[u8]) -> usize {
    // Discover the inclusive transition without knowing Group/Page layout.
    let (mut low, mut high) = (0, 4096);
    assert!(
        decode(
            wire,
            DecodeLimits {
                max_decoded_bytes: high,
                ..DecodeLimits::default()
            }
        )
        .is_ok()
    );
    while low < high {
        let mid = low + (high - low) / 2;
        if decode(
            wire,
            DecodeLimits {
                max_decoded_bytes: mid,
                ..DecodeLimits::default()
            },
        )
        .is_ok()
        {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

#[test]
fn structural_memory_budget_zero_low_and_exact_transition() {
    let singleton = reference_frame(&[ctid(0, 1)].into_iter().collect());
    let two_offsets = reference_frame(&[ctid(0, 1), ctid(0, 2)].into_iter().collect());
    let two_pages = reference_frame(&[ctid(0, 1), ctid(1, 1)].into_iter().collect());
    let two_groups = reference_frame(&[ctid(0, 1), ctid(256, 1)].into_iter().collect());
    let one_cost = minimum_storage(&singleton);
    assert!(one_cost > 2);
    assert_eq!(minimum_storage(&two_offsets), one_cost + 2);
    assert!(minimum_storage(&two_pages) > one_cost + 2);
    assert_eq!(minimum_storage(&two_groups), one_cost * 2);
    for wire in [singleton, two_offsets, two_pages, two_groups] {
        let exact = minimum_storage(&wire);
        for budget in [0, 1, exact - 1] {
            reject(
                "insufficient structural storage",
                &wire,
                DecodeLimits {
                    max_decoded_bytes: budget,
                    ..DecodeLimits::default()
                },
            );
        }
        assert!(
            decode(
                &wire,
                DecodeLimits {
                    max_decoded_bytes: exact,
                    ..DecodeLimits::default()
                }
            )
            .is_ok()
        );
    }
}

#[test]
fn malicious_declarations_at_and_above_default_bounds_do_not_panic() {
    let defaults = DecodeLimits::default();
    let body = group(0, &[(0, vec![1])]);
    for groups in [
        defaults.max_groups as u32,
        defaults.max_groups as u32 + 1,
        u32::MAX,
    ] {
        for postings in [
            defaults.max_postings as u64,
            defaults.max_postings as u64 + 1,
            u64::MAX,
        ] {
            for payload in [&[][..], body.as_slice()] {
                let bad = frame(payload, groups, postings);
                reject_sealed("huge declarations in tiny frame", &bad);
                // Broad limits cannot turn a tiny malformed declaration into
                // an unchecked reserve or overflow, either.
                reject(
                    "huge declarations with permissive budgets",
                    &bad,
                    DecodeLimits {
                        max_encoded_bytes: usize::MAX,
                        max_postings: usize::MAX,
                        max_groups: usize::MAX,
                        max_pages: usize::MAX,
                        max_decoded_bytes: usize::MAX,
                    },
                );
            }
        }
    }
}

fn exercise_bounded_bytes(bytes: &[u8]) {
    let limits = DecodeLimits {
        max_encoded_bytes: 4096,
        max_postings: 1024,
        max_groups: 64,
        max_pages: 256,
        max_decoded_bytes: 64 * 1024,
    };
    let result = catch_unwind(|| decode(bytes, limits));
    assert!(result.is_ok(), "panic for {} bytes", bytes.len());
    if let Ok(decoded) = result.unwrap() {
        let values: Vec<_> = decoded.iter().collect();
        assert_eq!(values.len(), decoded.len());
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            values
                .iter()
                .all(|value| value.block() != u32::MAX && value.offset() != 0)
        );
        let expected = values.into_iter().collect();
        assert_matches(&decoded, &expected);
        assert_eq!(
            reference_frame(&expected),
            bytes,
            "accepted a noncanonical representation"
        );
    }
}

#[test]
fn bounded_deterministic_random_bytes_and_resealed_valid_seed_mutations() {
    let mut rng = Generator(0xa110_cafe_0123_4567);
    for _ in 0..768 {
        let size = rng.next() as usize % 513;
        let bytes: Vec<_> = (0..size).map(|_| rng.next() as u8).collect();
        exercise_bounded_bytes(&bytes);
    }
    let seeds = [
        reference_frame(&BTreeSet::new()),
        reference_frame(&boundary_set()),
        reference_frame(&(0..256).map(|block| ctid(block, 1)).collect()),
        reference_frame(&[ctid(u32::MAX - 1, u16::MAX)].into_iter().collect()),
    ];
    for seed in seeds {
        exercise_bounded_bytes(&seed);
        for iteration in 0..192 {
            let mut mutated = seed.clone();
            let at = rng.next() as usize % mutated.len();
            mutated[at] ^= 1 << (rng.next() % 8);
            if iteration % 3 == 0 && mutated.len() > HEADER {
                let cut = HEADER + rng.next() as usize % (mutated.len() - HEADER);
                mutated.truncate(cut);
            } else if iteration % 3 == 1 {
                mutated.push(rng.next() as u8);
            }
            // Repair length as well as CRC to reach structure validation.
            if iteration % 2 == 0 {
                let length = (mutated.len() - HEADER) as u32;
                mutated[32..36].copy_from_slice(&length.to_le_bytes());
            }
            reseal(&mut mutated);
            assert_sealed(&mutated);
            exercise_bounded_bytes(&mutated);
        }
    }
}

#[test]
fn decoded_set_operations_match_independent_original_reference_sets() {
    for seed in 0..32 {
        let mut rng = Generator(seed + 17);
        let a: BTreeSet<_> = rng
            .values(if seed % 7 == 0 { 0 } else { 90 })
            .into_iter()
            .collect();
        let mut b: BTreeSet<_> = rng
            .values(if seed % 5 == 0 { 0 } else { 100 })
            .into_iter()
            .collect();
        // Guarantee shared offsets/pages/groups rather than relying on chance.
        b.extend(a.iter().step_by(3).copied());
        let decoded_a = decode(&reference_frame(&a), DecodeLimits::default()).unwrap();
        let decoded_b = decode(&reference_frame(&b), DecodeLimits::default()).unwrap();
        let cases = [
            (decoded_a.union(&decoded_b), a.union(&b).copied().collect()),
            (
                decoded_a.intersection(&decoded_b),
                a.intersection(&b).copied().collect(),
            ),
            (
                decoded_a.difference(&decoded_b),
                a.difference(&b).copied().collect(),
            ),
            (
                decoded_b.difference(&decoded_a),
                b.difference(&a).copied().collect(),
            ),
            (decoded_a.difference(&decoded_a), BTreeSet::new()),
        ];
        for (actual, expected) in cases {
            assert_matches(&actual, &expected);
            let wire = encode(&actual).unwrap();
            assert_matches(&decode(&wire, DecodeLimits::default()).unwrap(), &expected);
        }
    }
}
