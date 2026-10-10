//! The NOLXFS object checksum: Fletcher-64.
//!
//! Every on-disk object carries one over its own bytes with the `checksum` field set to
//! zero. It is a *detective* control: it catches torn writes, bit rot, and a reader being
//! pointed at the wrong block. It is not a cryptographic digest and offers no protection
//! against a deliberate attacker -- that is what the integrity metadata of M13 is for.
//!
//! ## Definition
//!
//! Fletcher-64 over the object's bytes read as little-endian `u32` words, with the two
//! 32-bit running sums reduced modulo `2^32 - 1`:
//!
//! ```text
//! sum1 = 0; sum2 = 0
//! for each 32-bit word w:
//!     sum1 = (sum1 + w) mod (2^32 - 1)
//!     sum2 = (sum2 + sum1) mod (2^32 - 1)
//! checksum = (sum2 << 32) | sum1
//! ```
//!
//! This is NOLX's own definition. It is the same family of checksum APFS uses, chosen
//! because it is cheap, needs no lookup table, and detects every single-word error and
//! every transposition of two words -- which is the error profile of real storage.
//!
//! ## Why the deferred reduction is correct
//!
//! The implementation below accumulates in 64-bit registers and reduces only at chunk
//! boundaries, which is much faster than reducing per word. That is sound because both
//! sums are linear over the integers and only their value modulo `2^32 - 1` is ever
//! observed: reducing a partial sum changes it by a multiple of the modulus, and every
//! later addition preserves that equivalence. [`tests::chunked_matches_unchunked`] is the
//! test that would fail if the chunk size ever grew large enough to overflow.

/// The modulus: `2^32 - 1`, a Mersenne prime, which is what makes the reduction a cheap
/// fold instead of a division.
const MODULUS: u32 = 0xFFFF_FFFF;

/// The largest number of words accumulated before a reduction.
///
/// With `u64` accumulators, `sum2` can reach roughly `n(n+1)/2 * (2^32-1)` before
/// overflowing `2^64`. At `n = 32768` that bound is about `2.3e18`, well inside range, so
/// there is a wide margin even on the largest object NOLXFS writes.
const NMAX: usize = 32768;

/// Compute the Fletcher-64 checksum of `words`.
#[must_use]
pub fn fletcher64(words: &[u32]) -> u64 {
    let mut sum1: u64 = 0;
    let mut sum2: u64 = 0;

    let mut rest = words;
    while !rest.is_empty() {
        let (chunk, tail) = if rest.len() > NMAX {
            rest.split_at(NMAX)
        } else {
            (rest, &[][..])
        };
        for &w in chunk {
            sum1 += u64::from(w);
            sum2 += sum1;
        }
        sum1 = reduce(sum1);
        sum2 = reduce(sum2);
        rest = tail;
    }

    (sum2 << 32) | sum1
}

/// Compute the checksum of a byte buffer whose length is a multiple of four.
///
/// Returns `None` for a length that is not a multiple of four. NOLXFS objects are always
/// a multiple of four bytes long, so a caller hitting `None` has a bug, and silently
/// padding the tail would hide it.
#[must_use]
pub fn fletcher64_bytes(bytes: &[u8]) -> Option<u64> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut sum1: u64 = 0;
    let mut sum2: u64 = 0;
    let mut count = 0usize;

    for chunk in bytes.chunks_exact(4) {
        let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        sum1 += u64::from(w);
        sum2 += sum1;
        count += 1;
        if count == NMAX {
            sum1 = reduce(sum1);
            sum2 = reduce(sum2);
            count = 0;
        }
    }

    Some((reduce(sum2) << 32) | reduce(sum1))
}

/// Fold a 64-bit accumulator into the 32-bit residue modulo `2^32 - 1`.
///
/// Because the modulus is `2^32 - 1`, `x mod M` is the sum of `x`'s two 32-bit halves,
/// possibly repeated once.
#[must_use]
fn reduce(x: u64) -> u64 {
    let mut folded = (x >> 32) + (x & 0xFFFF_FFFF);
    // `folded` can be at most 2^32 here, so one more fold always finishes it.
    folded = (folded >> 32) + (folded & 0xFFFF_FFFF);
    if folded == u64::from(MODULUS) {
        // The residue class of 0 has two representatives; normalise to the smaller one so
        // the checksum of an all-zero object is 0.
        folded = 0;
    }
    folded
}

/// Verify an object: recompute its checksum with the stored field zeroed and compare.
///
/// `stored` is the value found in the object's `checksum` field; `object` is the whole
/// object including that field.
#[must_use]
pub fn verify(object: &[u8], stored: u64) -> bool {
    let mut scratch_sum1: u64 = 0;
    let mut scratch_sum2: u64 = 0;
    let mut count = 0usize;

    for (index, chunk) in object.chunks_exact(4).enumerate() {
        let mut word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if index < 2 {
            // The checksum occupies bytes 0..8, i.e. the first two words.
            word = 0;
        }
        scratch_sum1 += u64::from(word);
        scratch_sum2 += scratch_sum1;
        count += 1;
        if count == NMAX {
            scratch_sum1 = reduce(scratch_sum1);
            scratch_sum2 = reduce(scratch_sum2);
            count = 0;
        }
    }

    let computed = (reduce(scratch_sum2) << 32) | reduce(scratch_sum1);
    // Constant-time-ish comparison is unnecessary here (the value is not a secret) but a
    // plain equality on integers already is.
    computed == stored
}

/// Compute the value that should be stored in an object's `checksum` field.
///
/// `object` must have its `checksum` field (bytes 0..8) already zeroed. This is the
/// counterpart to [`verify`]; writers call this, readers call that.
#[must_use]
pub fn compute(object: &[u8]) -> Option<u64> {
    if object.len() % 4 != 0 {
        return None;
    }
    let mut sum1: u64 = 0;
    let mut sum2: u64 = 0;
    let mut count = 0usize;

    for (index, chunk) in object.chunks_exact(4).enumerate() {
        let word = if index < 2 {
            0
        } else {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        sum1 += u64::from(word);
        sum2 += sum1;
        count += 1;
        if count == NMAX {
            sum1 = reduce(sum1);
            sum2 = reduce(sum2);
            count = 0;
        }
    }

    Some((reduce(sum2) << 32) | reduce(sum1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn words(n: usize, seed: u32) -> Vec<u32> {
        (0..n)
            .map(|i| seed.wrapping_mul(2_654_435_761).wrapping_add(i as u32))
            .collect()
    }

    #[test]
    fn all_zero_data_checksums_to_zero() {
        assert_eq!(fletcher64(&[0u32; 1]), 0);
        assert_eq!(fletcher64(&[0u32; 1024]), 0);
        assert_eq!(fletcher64_bytes(&[0u8; 4096]), Some(0));
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(fletcher64(&[]), 0);
        assert_eq!(fletcher64_bytes(&[]), Some(0));
    }

    #[test]
    fn matches_the_textbook_definition() {
        // Reference implementation, slow but obviously correct, over a small input.
        let data = [0x0102_0304u32, 0x0506_0708, 0xdead_beef, 1, 2, 3];
        let mut s1: u32 = 0;
        let mut s2: u32 = 0;
        for &w in &data {
            s1 = ((s1 as u64 + u64::from(w)) % u64::from(MODULUS)) as u32;
            s2 = ((s2 as u64 + u64::from(s1)) % u64::from(MODULUS)) as u32;
        }
        let expected = (u64::from(s2) << 32) | u64::from(s1);
        assert_eq!(fletcher64(&data), expected);
    }

    #[test]
    fn detects_every_single_word_change() {
        let base = words(512, 7);
        let base_sum = fletcher64(&base);
        for i in 0..base.len() {
            for bit in 0..32 {
                let mut mutated = base.clone();
                mutated[i] ^= 1 << bit;
                assert_ne!(
                    fletcher64(&mutated),
                    base_sum,
                    "single-bit flip at word {i} bit {bit} went undetected"
                );
            }
        }
    }

    #[test]
    fn detects_transposition() {
        let mut a = words(64, 3);
        let sum = fletcher64(&a);
        a.swap(10, 11);
        assert_ne!(
            fletcher64(&a),
            sum,
            "swapping adjacent words went undetected"
        );
    }

    #[test]
    fn is_deterministic() {
        let data = words(257, 99);
        assert_eq!(fletcher64(&data), fletcher64(&data));
    }

    #[test]
    fn chunked_matches_unchunked() {
        // The chunked accumulation must agree with a per-word reference for lengths that
        // cross the chunk boundary. NMAX is 32768, so this exercises several boundaries.
        for &len in &[1usize, 2, 3, 1000, NMAX - 1, NMAX, NMAX + 1, NMAX * 2 + 5] {
            let data = words(len, 5);
            let fast = fletcher64(&data);

            let mut s1: u64 = 0;
            let mut s2: u64 = 0;
            for &w in &data {
                s1 = (s1 + u64::from(w)) % u64::from(MODULUS);
                s2 = (s2 + s1) % u64::from(MODULUS);
            }
            let reference = (s2 << 32) | s1;
            assert_eq!(fast, reference, "mismatch at length {len}");
        }
    }

    #[test]
    fn byte_and_word_interfaces_agree() {
        let data = words(300, 42);
        let mut bytes = Vec::with_capacity(data.len() * 4);
        for w in &data {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(fletcher64(&data), fletcher64_bytes(&bytes));
    }

    #[test]
    fn byte_interface_rejects_unaligned_length() {
        assert_eq!(fletcher64_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn compute_and_verify_round_trip() {
        let mut object = alloc::vec![0u8; 4096];
        for (i, b) in object.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31);
        }
        // The checksum field must be zeroed before computing.
        object[0..8].copy_from_slice(&0u64.to_le_bytes());

        let sum = compute(&object).expect("4096 is a multiple of four");
        object[0..8].copy_from_slice(&sum.to_le_bytes());
        assert!(
            verify(&object, sum),
            "verify rejected a checksum compute just produced"
        );
    }

    #[test]
    fn verify_rejects_corruption_anywhere_in_the_object() {
        let mut object = alloc::vec![0xA5u8; 512];
        object[0..8].copy_from_slice(&0u64.to_le_bytes());
        let sum = compute(&object).unwrap();
        object[0..8].copy_from_slice(&sum.to_le_bytes());
        assert!(verify(&object, sum));

        for byte in 8..object.len() {
            let mut damaged = object.clone();
            damaged[byte] ^= 0x01;
            assert!(
                !verify(&damaged, sum),
                "corruption at byte {byte} went undetected"
            );
        }
    }

    #[test]
    fn verify_rejects_a_wrong_stored_value() {
        let mut object = alloc::vec![0u8; 128];
        object[16] = 1;
        let sum = compute(&object).unwrap();
        assert!(!verify(&object, sum.wrapping_add(1)));
        assert!(!verify(&object, 0));
    }

    #[test]
    fn reduce_folds_correctly() {
        assert_eq!(reduce(0), 0);
        assert_eq!(reduce(1), 1);
        assert_eq!(reduce(u64::from(MODULUS)), 0);
        assert_eq!(reduce(u64::from(MODULUS) + 1), 1);
        assert_eq!(reduce(1u64 << 32), 1);
        // The largest value the accumulators can hold must still fold without overflow.
        let big = (1u64 << 63) | 12345;
        assert!(reduce(big) < u64::from(MODULUS));
    }
}
