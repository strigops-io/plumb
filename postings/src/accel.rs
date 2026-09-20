// SPDX-License-Identifier: AGPL-3.0-or-later

//! Allocation-free checksums and four-word page masks.
//!
//! CRC32C (Castagnoli) and IEEE CRC32 use distinct reflected polynomials. Both
//! start with all bits set and complement the final state. CRC32C automatically
//! dispatches to SSE4.2 when available. Masks default to portable scalar Rust:
//! four-word AVX2 dispatch measured 1.8–1.9x slower on the benchmark host, so it
//! remains experimental opt-in rather than the production default. The compiler
//! may still vectorize portable Rust; "scalar" describes our dispatch policy.
//!
//! Only the private backend may use unsafe code. Experimental mask acceleration
//! uses runtime CPU/OS checks and falls back to scalar on unsupported targets.
//! Callers can preserve interrupt points with bounded checksum updates.

const CASTAGNOLI: u32 = 0x82f6_3b78;
const IEEE: u32 = 0xedb8_8320;

type Tables = [[u32; 256]; 8];

const fn tables(polynomial: u32) -> Tables {
    let mut result = [[0; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (polynomial & 0u32.wrapping_sub(crc & 1));
            bit += 1;
        }
        result[0][i] = crc;
        i += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        i = 0;
        while i < 256 {
            let crc = result[slice - 1][i];
            result[slice][i] = (crc >> 8) ^ result[0][(crc & 255) as usize];
            i += 1;
        }
        slice += 1;
    }
    result
}

static CRC32C_TABLES: Tables = tables(CASTAGNOLI);
static IEEE_TABLES: Tables = tables(IEEE);

// Slicing-by-eight is portable: byte indexing has no alignment or host-endian
// assumptions. chunks_exact guarantees every indexed byte exists.
fn update_table(mut crc: u32, bytes: &[u8], table: &Tables) -> u32 {
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let low = crc ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        crc = table[7][(low & 255) as usize]
            ^ table[6][((low >> 8) & 255) as usize]
            ^ table[5][((low >> 16) & 255) as usize]
            ^ table[4][(low >> 24) as usize]
            ^ table[3][usize::from(chunk[4])]
            ^ table[2][usize::from(chunk[5])]
            ^ table[1][usize::from(chunk[6])]
            ^ table[0][usize::from(chunk[7])];
    }
    for &byte in chunks.remainder() {
        crc = (crc >> 8) ^ table[0][((crc ^ u32::from(byte)) & 255) as usize];
    }
    crc
}

/// Incremental CRC32C; empty updates are harmless and finishing does not allocate.
#[derive(Clone, Debug)]
pub struct Crc32c {
    state: u32,
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    /// Start a Castagnoli checksum (initial state is all ones).
    pub fn new() -> Self {
        Self { state: u32::MAX }
    }

    /// Consume bytes in order, using SSE4.2 only after a runtime feature check.
    pub fn update(&mut self, bytes: &[u8]) {
        self.state = backend::crc32c_update(self.state, bytes);
    }

    /// Finish the checksum by complementing its state.
    pub fn finish(self) -> u32 {
        !self.state
    }
}

/// CRC32C with safe runtime dispatch.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(bytes);
    crc.finish()
}

/// Independent, bit-at-a-time CRC32C reference (not the fast table fallback).
pub fn crc32c_scalar(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (CASTAGNOLI & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// IEEE CRC32 over concatenated parts without copying or allocating.
///
/// This is NOT CRC32C: the SSE4.2 CRC instruction cannot compute this polynomial.
pub fn crc32_ieee_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    let mut crc = u32::MAX;
    for part in parts {
        crc = update_table(crc, part, &IEEE_TABLES);
    }
    !crc
}

/// Bitwise intersection of a 256-page mask using portable scalar Rust.
///
/// Avoids runtime dispatch overhead for this small, fixed-size operation.
pub fn mask_and(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    scalar_and(a, b)
}

/// Bitwise union of a 256-page mask using portable scalar Rust.
///
/// Avoids runtime dispatch overhead for this small, fixed-size operation.
pub fn mask_or(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    scalar_or(a, b)
}

/// Experimental intersection with runtime-guarded AVX2 and scalar fallback.
///
/// Opt-in for measurement, not a speed guarantee: four-word AVX2 dispatch was
/// slower than [`mask_and`] on the benchmark host. Safe on unsupported CPUs too.
pub fn mask_and_accelerated(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    backend::mask_and(a, b)
}

/// Experimental union with runtime-guarded AVX2 and scalar fallback.
///
/// Opt-in for measurement, not a speed guarantee: four-word AVX2 dispatch was
/// slower than [`mask_or`] on the benchmark host. Safe on unsupported CPUs too.
pub fn mask_or_accelerated(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    backend::mask_or(a, b)
}

/// Name of the runtime-selected CRC32C backend.
pub fn crc32c_backend() -> &'static str {
    if backend::has_crc32c() {
        "sse4.2"
    } else {
        "scalar-table"
    }
}

/// Backend policy used by [`mask_and`] and [`mask_or`], always `"scalar"`.
///
/// This names the portable Rust path, not instructions chosen by the compiler.
pub fn mask_backend() -> &'static str {
    "scalar"
}

/// Runtime-selected backend for the experimental accelerated mask functions.
///
/// Reports `"avx2"` only when the runtime CPU/OS check permits it, else `"scalar"`.
pub fn accelerated_mask_backend() -> &'static str {
    if backend::has_masks() {
        "avx2"
    } else {
        "scalar"
    }
}

fn scalar_and(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    std::array::from_fn(|i| a[i] & b[i])
}

fn scalar_or(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    std::array::from_fn(|i| a[i] | b[i])
}

// Safety boundary: the only unsafe allowance in the library. Safe entry points
// perform runtime CPU/OS checks before calling private target-feature routines.
// Architecture cfgs exclude all x86 intrinsics on other targets.
#[allow(unsafe_code)]
mod backend {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    pub(super) fn has_crc32c() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("sse4.2")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    pub(super) fn has_masks() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("avx2")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    pub(super) fn crc32c_update(crc: u32, bytes: &[u8]) -> u32 {
        #[cfg(target_arch = "x86_64")]
        if has_crc32c() {
            // SAFETY: the runtime check establishes the target feature.
            return unsafe { crc32c_sse42(crc, bytes) };
        }
        super::update_table(crc, bytes, &super::CRC32C_TABLES)
    }

    pub(super) fn mask_and(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
        #[cfg(target_arch = "x86_64")]
        if has_masks() {
            // SAFETY: runtime detection checks CPU and OS AVX support.
            return unsafe { masks_avx2::<true>(a, b) };
        }
        super::scalar_and(a, b)
    }

    pub(super) fn mask_or(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
        #[cfg(target_arch = "x86_64")]
        if has_masks() {
            // SAFETY: runtime detection checks CPU and OS AVX support.
            return unsafe { masks_avx2::<false>(a, b) };
        }
        super::scalar_or(a, b)
    }

    // Requires SSE4.2. Safe chunk decoding permits arbitrary slice alignment;
    // exact chunks and the remainder never read past the end, including empty.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse4.2")]
    unsafe fn crc32c_sse42(crc: u32, bytes: &[u8]) -> u32 {
        let mut wide = u64::from(crc);
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().expect("eight-byte chunk"));
            wide = _mm_crc32_u64(wide, word);
        }
        let mut crc = wide as u32;
        for &byte in chunks.remainder() {
            crc = _mm_crc32_u8(crc, byte);
        }
        crc
    }

    // Requires AVX2. Each by-value array contains exactly 32 initialized bytes.
    // loadu/storeu require no 32-byte alignment; the pointers stay within live
    // arrays and the output does not alias either input.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn masks_avx2<const AND: bool>(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
        let mut output = [0; 4];
        // SAFETY: all three arrays span the full 32-byte access, unaligned
        // accesses are supported, and the caller checked AVX2 availability.
        unsafe {
            let a = _mm256_loadu_si256(a.as_ptr().cast());
            let b = _mm256_loadu_si256(b.as_ptr().cast());
            let result = if AND {
                _mm256_and_si256(a, b)
            } else {
                _mm256_or_si256(a, b)
            };
            _mm256_storeu_si256(output.as_mut_ptr().cast(), result);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_table_and_masks_match_independent_references() {
        let data: Vec<_> = (0..4097).map(|i| (i * 37 + i / 13) as u8).collect();
        for start in 0..32 {
            for len in 0..=256 {
                let bytes = &data[start..start + len];
                assert_eq!(
                    !update_table(u32::MAX, bytes, &CRC32C_TABLES),
                    crc32c_scalar(bytes)
                );
            }
        }
        assert_eq!(
            !update_table(u32::MAX, &data, &CRC32C_TABLES),
            crc32c_scalar(&data)
        );
        let a = [0, 1, u64::MAX, 1 << 63];
        let b = [u64::MAX, 1 << 63, 7, 1];
        assert_eq!(mask_and(a, b), scalar_and(a, b));
        assert_eq!(mask_or(a, b), scalar_or(a, b));
    }
}
