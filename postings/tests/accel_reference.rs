// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeSet;

use plumb_postings::accel::{
    Crc32c, accelerated_mask_backend, crc32_ieee_parts, crc32c, crc32c_backend, crc32c_scalar,
    mask_and, mask_and_accelerated, mask_backend, mask_or, mask_or_accelerated,
};
use plumb_postings::{Ctid, Postings};

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn bytes(len: usize) -> Vec<u8> {
    let mut state = 0x614c_903b_a170_fff3;
    (0..len).map(|_| random(&mut state) as u8).collect()
}

fn ieee_reference(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[test]
fn known_vectors_and_backend_names() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(crc32c_scalar(b"123456789"), 0xe306_9283);
    assert_eq!(crc32_ieee_parts([b"123456789".as_slice()]), 0xcbf4_3926);
    assert_eq!(Crc32c::default().finish(), 0);
    assert_eq!(crc32c(b""), 0);
    assert_eq!(crc32_ieee_parts([]), 0);
    // The production policy is scalar even on AVX2-capable hosts.
    assert_eq!(mask_backend(), "scalar");
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(
            crc32c_backend(),
            if std::is_x86_feature_detected!("sse4.2") {
                "sse4.2"
            } else {
                "scalar-table"
            }
        );
        assert_eq!(
            accelerated_mask_backend(),
            if std::is_x86_feature_detected!("avx2") {
                "avx2"
            } else {
                "scalar"
            }
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        assert_eq!(crc32c_backend(), "scalar-table");
        assert_eq!(accelerated_mask_backend(), "scalar");
    }
}

#[test]
fn every_short_length_alignment_and_split() {
    let data = bytes(512);
    for start in 0..32 {
        for len in 0..=256 {
            let data = &data[start..start + len];
            let reference = crc32c_scalar(data);
            let ieee = ieee_reference(data);
            assert_eq!(crc32c(data), reference, "start={start} len={len}");
            assert_eq!(crc32_ieee_parts([data]), ieee);
            for split in 0..=len {
                let mut crc = Crc32c::new();
                crc.update(&data[..split]);
                crc.update(&[]);
                crc.update(&data[split..]);
                assert_eq!(
                    crc.finish(),
                    reference,
                    "start={start} len={len} split={split}"
                );
                assert_eq!(
                    crc32_ieee_parts([&data[..split], &[], &data[split..]]),
                    ieee
                );
            }
        }
    }
}

#[test]
fn deterministic_random_large_and_chunk_boundaries() {
    let data = bytes(1024 * 1024 + 65);
    let mut state = 0x5361_a01c_7e00_a188;
    let lengths = [
        257, 511, 512, 513, 4095, 4096, 4097, 65535, 65536, 65537, 1048576,
    ];
    for len in lengths
        .into_iter()
        .chain((0..64).map(|_| random(&mut state) as usize % 65537))
    {
        let start = len % 32;
        let data = &data[start..start + len];
        let expected = crc32c_scalar(data);
        let ieee = ieee_reference(data);
        assert_eq!(crc32c(data), expected);
        for chunk in [1, 7, 8, 9, 31, 64, 255, 4096, 65536] {
            let mut crc = Crc32c::new();
            for part in data.chunks(chunk) {
                crc.update(part);
            }
            assert_eq!(crc.finish(), expected, "len={len} chunk={chunk}");
            assert_eq!(crc32_ieee_parts(data.chunks(chunk)), ieee);
        }
    }
}

// Exercise both public policies against an independent word-wise reference.
// On AVX2 hosts the explicit accelerated calls execute the guarded vector path;
// elsewhere the same calls verify its safe scalar fallback.
fn check_masks(a: [u64; 4], b: [u64; 4]) {
    let and = std::array::from_fn(|i| a[i] & b[i]);
    let or = std::array::from_fn(|i| a[i] | b[i]);
    assert_eq!(mask_and(a, b), and);
    assert_eq!(mask_or(a, b), or);
    assert_eq!(mask_and_accelerated(a, b), and);
    assert_eq!(mask_or_accelerated(a, b), or);
}

#[test]
fn default_and_experimental_masks_match_scalar_for_every_bit_and_random_masks() {
    let mut state = 0x2234_efe3_971d_047a;
    let mut check = |a: [u64; 4], b: [u64; 4]| {
        check_masks(a, b);
        let c = std::array::from_fn(|_| random(&mut state));
        check_masks(a, c);
    };
    check([0; 4], [u64::MAX; 4]);
    check([u64::MAX; 4], [u64::MAX; 4]);
    for bit in 0..256 {
        let mut a = [0; 4];
        a[bit / 64] = 1 << (bit % 64);
        for other in 0..256 {
            let mut b = [0; 4];
            b[other / 64] = 1 << (other % 64);
            check(a, b);
        }
    }
    for _ in 0..10000 {
        let a = std::array::from_fn(|_| random(&mut state));
        let b = std::array::from_fn(|_| random(&mut state));
        check_masks(a, b);
    }
}

#[test]
fn group_word_and_block_boundaries_preserve_set_semantics() {
    let mut state = 0xa63f_c319_222a_6791;
    for base in [0, 256, 512, u32::MAX - 255] {
        let mut a = BTreeSet::new();
        let mut b = BTreeSet::new();
        for page in 0..256 {
            let block = base + page;
            if block == u32::MAX {
                continue;
            }
            for offset in [1, 2, 3, u16::MAX] {
                let ctid = Ctid::new(block, offset).unwrap();
                if random(&mut state) & 1 != 0 {
                    a.insert(ctid);
                }
                if random(&mut state) & 1 != 0 {
                    b.insert(ctid);
                }
            }
        }
        let left = Postings::from_ctids(a.iter().copied());
        let right = Postings::from_ctids(b.iter().copied());
        for (actual, expected) in [
            (left.union(&right), a.union(&b).copied().collect::<Vec<_>>()),
            (
                left.intersection(&right),
                a.intersection(&b).copied().collect(),
            ),
            (left.difference(&right), a.difference(&b).copied().collect()),
            (right.difference(&left), b.difference(&a).copied().collect()),
        ] {
            assert_eq!(actual.iter().collect::<Vec<_>>(), expected);
        }
    }
}
