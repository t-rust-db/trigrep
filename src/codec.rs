// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Trigram packing and the posting-list codec (#34).
//!
//! A trigram is any 3-byte sliding window over a file's bytes (not chars:
//! UTF-8 is self-synchronising, so a byte-window index finds every
//! multi-byte literal too, tgrep/codesearch do the same). It packs into
//! an `i64` rowid big-endian, so the `trigrams` b-tree is ordered the
//! same way `memcmp` would order the trigrams themselves.
//!
//! A posting list is the sorted set of file ids containing a trigram,
//! stored as LEB128 varints of the *gaps* between consecutive ids
//! (delta-varint, the encoding tgrep uses). Ids start at 1 so the first
//! gap is the first id itself.

use std::fmt;

/// Packs a 3-byte window into the rowid the `trigrams` table is keyed by.
pub fn pack(t: [u8; 3]) -> i64 {
    i64::from(u32::from_be_bytes([0, t[0], t[1], t[2]]))
}

/// Every distinct trigram of `bytes`, sorted ascending. Inputs shorter
/// than three bytes have none.
///
/// Two strategies, same result. Small inputs collect-sort-dedup. Large
/// inputs (#3) mark a 2^24-bit bitmap instead: collecting first costs
/// 8 bytes per input byte *before* dedup — a 7.7 MB file became a 62 MB
/// vector, and with eight reader threads in flight that, not the pager,
/// was the build's peak RSS. The bitmap is a flat 2 MiB whatever the
/// input, and scanning it yields the trigrams already sorted.
pub fn unique_trigrams(bytes: &[u8]) -> Vec<i64> {
    const BITMAP_THRESHOLD: usize = 256 << 10;
    if bytes.len() < BITMAP_THRESHOLD {
        let mut out: Vec<i64> = bytes
            .windows(3)
            .filter_map(|w| match *w {
                [a, b, c] => Some(pack([a, b, c])),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        return out;
    }
    let mut bits = vec![0u64; (1usize << 24) / 64];
    for w in bytes.windows(3) {
        let Some(&[a, b, c]) = w.first_chunk::<3>() else {
            continue;
        };
        let t = pack([a, b, c]);
        let idx = usize::try_from(t).unwrap_or(0);
        let (word, bit) = (idx / 64, idx % 64);
        if let Some(slot) = bits.get_mut(word) {
            *slot |= 1u64.wrapping_shl(u32::try_from(bit).unwrap_or(0));
        }
    }
    let mut out = Vec::new();
    for (wi, &word) in bits.iter().enumerate() {
        let mut w = word;
        while w != 0 {
            let bit = w.trailing_zeros();
            let t = i64::try_from(wi)
                .unwrap_or(0)
                .wrapping_mul(64)
                .wrapping_add(i64::from(bit));
            out.push(t);
            w &= w.wrapping_sub(1);
        }
    }
    out
}

/// A posting list that failed to decode — the cache file is damaged
/// (or was written by a newer `trigrep`); a `--rebuild` fixes either.
#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    /// A varint ran past the end of the blob.
    Truncated,
    /// A varint was longer than the 10 bytes a `u64` can need.
    Overlong,
    /// A gap took the running id past `i64::MAX`.
    Overflow,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated => f.write_str("posting list truncated"),
            CodecError::Overlong => f.write_str("posting list varint too long"),
            CodecError::Overflow => f.write_str("posting list file id overflow"),
        }
    }
}

impl std::error::Error for CodecError {}

fn push_leb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Encodes a sorted, duplicate-free list of positive file ids.
/// Callers hold the sorted-unique invariant (`merge_into` preserves it);
/// an unsorted input still round-trips through `decode_postings` only if
/// no gap is negative, so keep them sorted.
pub fn encode_postings(ids: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len().saturating_mul(2));
    let mut prev: i64 = 0;
    for &id in ids {
        let gap = id.wrapping_sub(prev);
        push_leb128(&mut out, gap.cast_unsigned());
        prev = id;
    }
    out
}

/// Decodes a blob written by [`encode_postings`].
pub fn decode_postings(buf: &[u8]) -> Result<Vec<i64>, CodecError> {
    let mut ids = Vec::new();
    let mut prev: i64 = 0;
    let mut iter = buf.iter();
    loop {
        let mut gap: u64 = 0;
        let mut shift: u32 = 0;
        let mut started = false;
        loop {
            let Some(&byte) = iter.next() else {
                if started {
                    return Err(CodecError::Truncated);
                }
                return Ok(ids);
            };
            started = true;
            if shift >= 64 {
                return Err(CodecError::Overlong);
            }
            // The 10th byte sits at shift 63: only its low bit fits a u64.
            // Any higher payload bit would be shifted out silently (#13).
            if shift == 63 && byte & 0x7e != 0 {
                return Err(CodecError::Overlong);
            }
            gap |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift = shift.saturating_add(7);
        }
        let gap = i64::try_from(gap).map_err(|_| CodecError::Overflow)?;
        let id = prev.checked_add(gap).ok_or(CodecError::Overflow)?;
        ids.push(id);
        prev = id;
    }
}

/// Sorted-merge intersection of two ascending id lists.
pub fn intersect(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut ia, mut ib) = (a.iter().peekable(), b.iter().peekable());
    while let (Some(&&x), Some(&&y)) = (ia.peek(), ib.peek()) {
        match x.cmp(&y) {
            std::cmp::Ordering::Less => {
                ia.next();
            }
            std::cmp::Ordering::Greater => {
                ib.next();
            }
            std::cmp::Ordering::Equal => {
                out.push(x);
                ia.next();
                ib.next();
            }
        }
    }
    out
}

/// Merges sorted `add` into sorted `into`, keeping it sorted and unique.
/// Returns whether anything changed — an unchanged posting list must not
/// be rewritten (that is the whole point of batching per trigram).
pub fn merge_into(into: &mut Vec<i64>, add: &[i64]) -> bool {
    let before = into.len();
    into.extend(add.iter().copied());
    into.sort_unstable();
    into.dedup();
    into.len() != before
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    proptest! {
        /// #14: encode→decode is the identity on any sorted, unique, positive id list.
        #[test]
        fn postings_round_trip(mut ids in proptest::collection::vec(1i64..=(1i64 << 40), 0..200)) {
            ids.sort_unstable();
            ids.dedup();
            let blob = super::encode_postings(&ids);
            prop_assert_eq!(super::decode_postings(&blob).unwrap(), ids);
        }

        /// #14: decoding arbitrary bytes never panics — it returns Ok or a CodecError.
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            drop(super::decode_postings(&bytes));
        }

        /// #14: the two unique_trigrams strategies agree on both sides of the
        /// 256 KiB threshold, for dense and sparse alphabets and tiny inputs.
        #[test]
        fn unique_trigrams_strategies_agree(
            len in prop_oneof![0usize..8, (256usize << 10) - 4..(256usize << 10) + 4, 300usize << 10..(300usize << 10) + 2],
            alphabet in 1u8..=255,
            seed in any::<u64>(),
        ) {
            let mut x = seed | 1;
            let bytes: Vec<u8> = (0..len).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x % u64::from(alphabet)) as u8 }).collect();
            let got = super::unique_trigrams(&bytes);
            let mut want: Vec<i64> = bytes.windows(3).map(|w| super::pack([w[0], w[1], w[2]])).collect();
            want.sort_unstable();
            want.dedup();
            prop_assert_eq!(got, want);
        }
    }
    #[test]
    fn tenth_varint_byte_with_high_payload_bits_is_overlong() {
        // nine continuation bytes (shift reaches 63), then 0x7e: bits 1-6 set.
        let mut buf = vec![0x80u8; 9];
        buf.push(0x7e);
        assert_eq!(
            super::decode_postings(&buf),
            Err(super::CodecError::Overlong)
        );
        // ...whereas a tenth byte of exactly 0x01 is the legal top bit.
        let mut ok = vec![0x80u8; 9];
        ok.push(0x01);
        // 1 << 63 does not fit i64 → Overflow, never a silent wrong id.
        assert_eq!(
            super::decode_postings(&ok),
            Err(super::CodecError::Overflow)
        );
    }
    #[test]
    fn bitmap_and_collect_paths_agree_on_large_input() {
        // Deterministic pseudo-random bytes past the bitmap threshold.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let bytes: Vec<u8> = (0..(300usize << 10))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0x3f) as u8 + b'0' // 64-symbol alphabet keeps the set dense but not full
            })
            .collect();
        let via_bitmap = super::unique_trigrams(&bytes);
        let mut via_collect: Vec<i64> = bytes
            .windows(3)
            .map(|w| super::pack([w[0], w[1], w[2]]))
            .collect();
        via_collect.sort_unstable();
        via_collect.dedup();
        assert_eq!(via_bitmap, via_collect);
        assert!(via_bitmap.windows(2).all(|p| p[0] < p[1]));
    }
    use super::*;

    #[test]
    fn pack_is_big_endian_and_orders_like_memcmp() {
        assert_eq!(pack([0, 0, 1]), 1);
        assert_eq!(pack([1, 0, 0]), 0x01_00_00);
        assert!(pack(*b"abc") < pack(*b"abd"));
        assert!(pack(*b"abz") < pack(*b"aca"));
    }

    #[test]
    fn unique_trigrams_dedups_and_sorts() {
        assert_eq!(unique_trigrams(b"ab"), Vec::<i64>::new());
        assert_eq!(unique_trigrams(b"abc"), vec![pack(*b"abc")]);
        let t = unique_trigrams(b"abcabc");
        assert_eq!(t, vec![pack(*b"abc"), pack(*b"bca"), pack(*b"cab")]);
    }

    #[test]
    fn postings_roundtrip() {
        for ids in [
            vec![],
            vec![1],
            vec![1, 2, 3],
            vec![1, 200, 300_000, 5_000_000_000],
            vec![i64::MAX],
        ] {
            let blob = encode_postings(&ids);
            assert_eq!(decode_postings(&blob), Ok(ids));
        }
    }

    #[test]
    fn postings_gap_encoding_is_compact() {
        // 1000 consecutive ids: every gap is 1, one byte each.
        let ids: Vec<i64> = (1..=1000).collect();
        assert_eq!(encode_postings(&ids).len(), 1000);
    }

    #[test]
    fn decode_rejects_truncated_and_overlong() {
        assert_eq!(decode_postings(&[0x80]), Err(CodecError::Truncated));
        assert_eq!(decode_postings(&[0xff; 11]), Err(CodecError::Overlong));
        // 0xff... exactly 10 bytes decodes to u64::MAX which overflows i64.
        let mut ten = vec![0xff; 9];
        ten.push(0x01);
        assert_eq!(decode_postings(&ten), Err(CodecError::Overflow));
    }

    #[test]
    fn intersect_and_merge() {
        assert_eq!(intersect(&[1, 3, 5, 7], &[3, 4, 5, 8]), vec![3, 5]);
        assert_eq!(intersect(&[], &[1]), Vec::<i64>::new());
        let mut v = vec![1, 3];
        assert!(merge_into(&mut v, &[2, 3]));
        assert_eq!(v, vec![1, 2, 3]);
        assert!(!merge_into(&mut v, &[2]));
    }
}
