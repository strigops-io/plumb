// SPDX-License-Identifier: AGPL-3.0-or-later

//! Allocation-contract regression tests for docs/postings-format.md.
//! The library remains `forbid(unsafe_code)`. Only this standalone test binary's
//! allocator forwarding module permits unsafe; it never inspects allocated memory.

#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

use std::mem::size_of;

use plumb_postings::codec::{CodecError, DecodeLimits, decode, encode};
use plumb_postings::{Ctid, Postings};

#[allow(unsafe_code)]
mod allocation_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::ptr;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Stats {
        pub attempts: usize,
        pub successes: usize,
        pub deallocations: usize,
        pub requested_bytes: usize,
        pub allocated_bytes: usize,
        pub freed_bytes: usize,
        pub reallocations: usize,
        pub sizes: [usize; 8],
    }

    #[derive(Clone, Copy)]
    struct State {
        active: bool,
        fail_at: Option<usize>,
        stats: Stats,
    }

    impl State {
        const IDLE: Self = Self {
            active: false,
            fail_at: None,
            stats: Stats {
                attempts: 0,
                successes: 0,
                deallocations: 0,
                requested_bytes: 0,
                allocated_bytes: 0,
                freed_bytes: 0,
                reallocations: 0,
                sizes: [0; 8],
            },
        };
    }

    thread_local! {
        // Const initialization and a Copy, destructor-free Cell avoid allocator
        // recursion. Other harness threads never see this thread's active flag.
        static STATE: Cell<State> = const { Cell::new(State::IDLE) };
    }

    fn update(f: impl FnOnce(&mut State)) {
        // During TLS teardown, simply stop observing and keep forwarding to System.
        let _ = STATE.try_with(|cell| {
            let mut state = cell.get();
            if state.active {
                f(&mut state);
                cell.set(state);
            }
        });
    }

    fn reject(size: usize, reallocation: bool) -> bool {
        let mut reject = false;
        update(|state| {
            let index = state.stats.attempts;
            if let Some(slot) = state.stats.sizes.get_mut(index) {
                *slot = size;
            }
            state.stats.attempts = index.saturating_add(1);
            state.stats.requested_bytes = state.stats.requested_bytes.saturating_add(size);
            state.stats.reallocations = state
                .stats
                .reallocations
                .saturating_add(usize::from(reallocation));
            reject = state.fail_at == Some(index);
        });
        reject
    }

    fn allocated(size: usize, pointer: *mut u8) {
        if !pointer.is_null() {
            update(|state| {
                state.stats.successes = state.stats.successes.saturating_add(1);
                state.stats.allocated_bytes = state.stats.allocated_bytes.saturating_add(size);
            });
        }
    }

    fn freed(size: usize) {
        update(|state| {
            state.stats.deallocations = state.stats.deallocations.saturating_add(1);
            state.stats.freed_bytes = state.stats.freed_bytes.saturating_add(size);
        });
    }

    struct Allocator;

    // SAFETY: No bookkeeping allocates, panics, dereferences pointers, or unwinds.
    // All successful allocation operations and every deallocation are forwarded
    // unchanged to System. Injected null results obey GlobalAlloc's failure rules.
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if reject(layout.size(), false) {
                return ptr::null_mut();
            }
            // SAFETY: GlobalAlloc's caller supplies a valid, nonzero layout.
            let pointer = unsafe { System.alloc(layout) };
            allocated(layout.size(), pointer);
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if reject(layout.size(), false) {
                return ptr::null_mut();
            }
            // SAFETY: Preserve the caller's layout and System's zeroing contract.
            let pointer = unsafe { System.alloc_zeroed(layout) };
            allocated(layout.size(), pointer);
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            freed(layout.size());
            // SAFETY: The caller supplies a live System allocation and its layout.
            unsafe { System.dealloc(pointer, layout) };
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            if reject(size, true) {
                // A failed realloc MUST leave the original allocation untouched.
                return ptr::null_mut();
            }
            // SAFETY: Forward the original pointer/layout and valid new size.
            let replacement = unsafe { System.realloc(pointer, layout, size) };
            if !replacement.is_null() {
                freed(layout.size());
            }
            allocated(size, replacement);
            replacement
        }
    }

    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;

    struct Guard;

    impl Guard {
        fn start(fail_at: Option<usize>) -> Self {
            // Assertions are outside allocator callbacks and before activation.
            STATE.with(|cell| {
                assert!(!cell.get().active, "allocation probes cannot nest");
                cell.set(State {
                    active: true,
                    fail_at,
                    ..State::IDLE
                });
            });
            Self
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = STATE.try_with(|cell| cell.set(State::IDLE));
        }
    }

    pub fn measure<T>(fail_at: Option<usize>, operation: impl FnOnce() -> T) -> (T, Stats) {
        let guard = Guard::start(fail_at);
        let result = operation();
        let stats = STATE.with(|cell| cell.get().stats);
        drop(guard); // Also runs during unwinding; assertions occur after this point.
        (result, stats)
    }

    pub fn is_idle() -> bool {
        STATE.with(|cell| !cell.get().active && cell.get().stats == Stats::default())
    }
}

use allocation_probe::{Stats, measure};

// Safe size-only mirrors of the private representation, not casts or an ABI
// promise. Keep these fields in sync with src/lib.rs when representation changes.
// No hard-coded 64-bit sizes: the resource contract uses native element sizes.
#[allow(dead_code)]
struct GroupStorage {
    base: u32,
    mask: [u64; 4],
    pages: Vec<PageStorage>,
}

#[allow(dead_code)]
struct PageStorage {
    index: u8,
    offsets: Vec<u16>,
}

fn structural_bytes() -> usize {
    2 * size_of::<GroupStorage>() + 3 * size_of::<PageStorage>() + 4 * size_of::<u16>()
}

fn fixture() -> (Postings, Vec<u8>) {
    // Two groups, three pages, four offsets; group 1 starts at byte 86.
    let postings = Postings::from_ctids([
        Ctid::new(0, 1).unwrap(),
        Ctid::new(0, 2).unwrap(),
        Ctid::new(255, 1).unwrap(),
        Ctid::new(256, 1).unwrap(),
    ]);
    let bytes = encode(&postings).unwrap();
    assert_eq!(bytes.len(), 126);
    (postings, bytes)
}

fn assert_released(stats: Stats) {
    assert_eq!(stats.successes, stats.deallocations, "{stats:?}");
    assert_eq!(stats.allocated_bytes, stats.freed_bytes, "{stats:?}");
    assert_eq!(stats.reallocations, 0, "exact reservations must not grow");
}

#[test]
fn decode_six_exact_allocations_and_inclusive_structural_budget() {
    let (expected, bytes) = fixture();
    let budget = structural_bytes();
    let sizes = [
        2 * size_of::<GroupStorage>(),
        2 * size_of::<PageStorage>(),
        2 * size_of::<u16>(),
        size_of::<u16>(),
        size_of::<PageStorage>(),
        size_of::<u16>(),
        0,
        0,
    ];
    let limits = DecodeLimits {
        max_encoded_bytes: bytes.len(),
        max_groups: 2,
        max_pages: 3,
        max_postings: 4,
        max_decoded_bytes: budget,
    };
    let (matches, stats) = measure(None, || {
        let result = decode(&bytes, limits);
        let matches = result.as_ref().is_ok_and(|decoded| decoded == &expected);
        drop(result); // Include destruction so every successful request is balanced.
        matches
    });
    assert!(matches);
    assert_eq!(stats.attempts, 6); // groups Vec + two pages Vec + three offsets Vec
    assert_eq!(stats.successes, 6);
    assert_eq!(stats.sizes, sizes);
    assert_eq!(stats.requested_bytes, budget);
    assert_released(stats);

    assert_allocation_free_error(
        &bytes,
        DecodeLimits {
            max_decoded_bytes: budget - 1,
            ..limits
        },
        CodecError::DecodedBytesLimit,
    );
}

#[test]
fn decode_failure_at_each_allocation_rolls_back() {
    let (expected, bytes) = fixture();
    let limits = DecodeLimits::default();
    for fail_at in 0..6 {
        let (result, stats) = measure(Some(fail_at), || decode(&bytes, limits));
        assert_eq!(result, Err(CodecError::AllocationFailed), "index {fail_at}");
        assert_eq!(stats.attempts, fail_at + 1);
        assert_eq!(stats.successes, fail_at);
        assert_released(stats);
        assert!(allocation_probe::is_idle());
        // A failed construction neither changes the borrowed frame nor poisons TLS.
        assert_eq!(decode(&bytes, limits).unwrap(), expected);
    }
}

#[test]
fn encode_has_one_exact_allocation_and_handles_its_failure() {
    let (postings, expected) = fixture();
    let (matches, stats) = measure(None, || {
        let result = encode(&postings);
        let matches = result.as_ref().is_ok_and(|bytes| bytes == &expected);
        drop(result);
        matches
    });
    assert!(matches);
    assert_eq!(stats.attempts, 1);
    assert_eq!(stats.successes, 1);
    assert_eq!(stats.requested_bytes, expected.len());
    assert_released(stats);

    let (result, stats) = measure(Some(0), || encode(&postings));
    assert_eq!(result, Err(CodecError::AllocationFailed));
    assert_eq!(stats.attempts, 1);
    assert_eq!(stats.successes, 0);
    assert_released(stats);
    assert_eq!(encode(&postings).unwrap(), expected);
}

fn assert_allocation_free_error(bytes: &[u8], limits: DecodeLimits, expected: CodecError) {
    // Fail the very first request as a fail-safe: even absurd declarations never
    // reach System as huge allocations if validation regresses.
    let (result, stats) = measure(Some(0), || decode(bytes, limits));
    assert_eq!(result, Err(expected));
    assert_eq!(stats, Stats::default());
}

fn reseal(bytes: &mut [u8]) {
    // Independent reflected Castagnoli CRC32C, omitting the checksum field.
    let mut crc = !0u32;
    for &byte in bytes[..36].iter().chain(&bytes[40..]) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0x82f6_3b78 } else { 0 };
        }
    }
    bytes[36..40].copy_from_slice(&(!crc).to_le_bytes());
}

#[test]
fn invalid_magic_and_checksum_never_allocate() {
    let (_, mut bytes) = fixture();
    bytes[0] ^= 1;
    assert_allocation_free_error(&bytes, DecodeLimits::default(), CodecError::InvalidMagic);
    bytes[0] ^= 1;
    bytes[36] ^= 1;
    assert_allocation_free_error(
        &bytes,
        DecodeLimits::default(),
        CodecError::ChecksumMismatch,
    );
}

#[test]
fn resealed_structural_corruptions_never_allocate() {
    let (_, original) = fixture();
    // Late failures are important: validation must finish before allocating even
    // when the entire first group and its pages are otherwise valid.
    let cases: &[(usize, &[u8], CodecError)] = &[
        (40, &1u32.to_le_bytes(), CodecError::UnalignedGroupBase),
        (44, &[0; 32], CodecError::EmptyGroup),
        (76, &0u16.to_le_bytes(), CodecError::EmptyPage),
        (78, &0u16.to_le_bytes(), CodecError::ZeroOffset),
        (80, &1u16.to_le_bytes(), CodecError::OffsetOrder),
        (86, &0u32.to_le_bytes(), CodecError::GroupOrder),
        (122, &0u16.to_le_bytes(), CodecError::EmptyPage),
        (124, &0u16.to_le_bytes(), CodecError::ZeroOffset),
        (24, &5u64.to_le_bytes(), CodecError::PostingCountMismatch),
        (16, &1u32.to_le_bytes(), CodecError::LengthMismatch),
    ];
    for &(position, replacement, expected) in cases {
        let mut bytes = original.clone();
        bytes[position..position + replacement.len()].copy_from_slice(replacement);
        reseal(&mut bytes);
        assert_allocation_free_error(&bytes, DecodeLimits::default(), expected);
    }
}

#[test]
fn oversized_declarations_and_caller_limits_never_allocate() {
    let (_, original) = fixture();
    let defaults = DecodeLimits::default();
    let cases: &[(usize, &[u8], CodecError)] = &[
        (16, &u32::MAX.to_le_bytes(), CodecError::GroupsLimit),
        // u32::MAX is representable by usize on both 32- and 64-bit targets.
        (
            24,
            &u64::from(u32::MAX).to_le_bytes(),
            CodecError::PostingsLimit,
        ),
    ];
    for &(position, replacement, expected) in cases {
        let mut bytes = original.clone();
        bytes[position..position + replacement.len()].copy_from_slice(replacement);
        reseal(&mut bytes);
        assert_allocation_free_error(&bytes, defaults, expected);
    }
    // Even if counts are permitted by the caller, tiny bodies cannot substantiate
    // huge declarations. No huge buffers are ever constructed by these tests.
    let unlimited = DecodeLimits {
        max_encoded_bytes: usize::MAX,
        max_groups: usize::MAX,
        max_pages: usize::MAX,
        max_postings: usize::MAX,
        max_decoded_bytes: usize::MAX,
    };
    let mut bytes = original.clone();
    bytes[24..32].copy_from_slice(&u64::from(u32::MAX).to_le_bytes());
    reseal(&mut bytes);
    assert_allocation_free_error(&bytes, unlimited, CodecError::PostingCountMismatch);

    for (limits, expected) in [
        (
            DecodeLimits {
                max_encoded_bytes: original.len() - 1,
                ..defaults
            },
            CodecError::EncodedBytesLimit,
        ),
        (
            DecodeLimits {
                max_groups: 1,
                ..defaults
            },
            CodecError::GroupsLimit,
        ),
        (
            DecodeLimits {
                max_pages: 2,
                ..defaults
            },
            CodecError::PagesLimit,
        ),
        (
            DecodeLimits {
                max_postings: 3,
                ..defaults
            },
            CodecError::PostingsLimit,
        ),
        (
            DecodeLimits {
                max_decoded_bytes: 0,
                ..defaults
            },
            CodecError::DecodedBytesLimit,
        ),
    ] {
        assert_allocation_free_error(&original, limits, expected);
    }
}

#[test]
fn empty_decode_needs_no_allocation_or_structural_budget() {
    let expected = Postings::new();
    let bytes = encode(&expected).unwrap();
    let limits = DecodeLimits {
        max_encoded_bytes: bytes.len(),
        max_groups: 0,
        max_pages: 0,
        max_postings: 0,
        max_decoded_bytes: 0,
    };
    let (result, stats) = measure(Some(0), || decode(&bytes, limits));
    assert_eq!(result, Ok(expected));
    assert_eq!(stats, Stats::default());
}

#[test]
fn probe_resets_on_unwind_without_touching_the_global_panic_hook() {
    // Prepare the panic payload before instrumentation. resume_unwind exercises
    // RAII without invoking a global hook that might affect parallel tests.
    let payload = Box::new("probe reset");
    let result = std::panic::catch_unwind(|| {
        // The unwinder itself may allocate, so count but do not fail its requests.
        measure(None, || std::panic::resume_unwind(payload));
    });
    assert!(result.is_err());
    assert!(allocation_probe::is_idle());
    let (postings, bytes) = fixture();
    assert_eq!(decode(&bytes, DecodeLimits::default()).unwrap(), postings);
}
