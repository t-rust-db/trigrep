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
pub fn unique_trigrams(bytes: &[u8]) -> Vec<i64> {
    let mut out: Vec<i64> = bytes
        .windows(3)
        .filter_map(|w| match *w {
            [a, b, c] => Some(pack([a, b, c])),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
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
        push_leb128(&mut out, gap as u64);
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
