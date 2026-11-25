//! Compact string interning (`StringPool`) and prefix-trie linearized
//! front-coding (`FrontCodedStrings`).
//!
//! Filesystems repeat basenames millions of times (`index.js`, `Cargo.toml`,
//! `.git`, `mod.rs`) and share long common prefixes. Interning into a single
//! contiguous buffer eliminates per-node heap allocations in RAM, and
//! front-coding the lexicographically sorted dictionary achieves prefix-trie
//! compression on disk with O(N) single-pass decoding.

use std::hash::{BuildHasher as _, Hasher as _};
use std::io;

use rustc_hash::{FxBuildHasher, FxHashMap};

/// Dense 4-byte index into a [`StringPool`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

impl NameId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Contiguous, deduplicated string pool with zero per-string heap allocations.
///
/// Strings live back-to-back in `data`; `offsets` has length `len() + 1`, so
/// entry `i` is `&data[offsets[i]..offsets[i + 1]]`. Lookup uses 64-bit `FxHash`
/// buckets without storing a second copy of any string.
#[derive(Clone, Debug)]
pub struct StringPool {
    data: String,
    offsets: Vec<u32>,
    /// Primary hash -> first `NameId` with that hash.
    index: FxHashMap<u64, NameId>,
    /// Rare hash-collision overflow list.
    collisions: Vec<(u64, NameId)>,
}

impl Default for StringPool {
    fn default() -> Self {
        Self::new()
    }
}

impl StringPool {
    pub fn new() -> Self {
        Self {
            data: String::new(),
            offsets: vec![0],
            index: FxHashMap::default(),
            collisions: Vec::new(),
        }
    }

    pub fn with_capacity(strings: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(strings + 1);
        offsets.push(0);
        Self {
            data: String::with_capacity(bytes),
            offsets,
            index: FxHashMap::with_capacity_and_hasher(strings, FxBuildHasher),
            collisions: Vec::new(),
        }
    }

    /// Number of unique interned strings.
    pub const fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total UTF-8 bytes stored in the contiguous backing buffer.
    pub const fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Intern `value`, returning its existing [`NameId`] or inserting it once.
    pub fn intern(&mut self, value: &str) -> NameId {
        let hash = Self::hash_str(value);
        if let Some(&existing) = self.index.get(&hash) {
            if self.get(existing) == value {
                return existing;
            }
            for &(collision_hash, id) in &self.collisions {
                if collision_hash == hash && self.get(id) == value {
                    return id;
                }
            }
            let id = self.push_raw(value);
            self.collisions.push((hash, id));
            return id;
        }
        let id = self.push_raw(value);
        self.index.insert(hash, id);
        id
    }

    /// Append a string known to be unique.
    fn push_raw(&mut self, value: &str) -> NameId {
        let id = NameId(self.len() as u32);
        self.data.push_str(value);
        self.offsets.push(self.data.len() as u32);
        id
    }

    /// Look up the string for `id`. Returns `""` if out of bounds.
    pub fn get(&self, id: NameId) -> &str {
        let idx = id.index();
        let Some(&start) = self.offsets.get(idx) else {
            return "";
        };
        let Some(&end) = self.offsets.get(idx + 1) else {
            return "";
        };
        &self.data[start as usize..end as usize]
    }

    /// Iterate over every interned string in ID order.
    pub fn iter(&self) -> impl Iterator<Item = (NameId, &str)> {
        (0..self.len()).map(|idx| {
            let id = NameId(idx as u32);
            (id, self.get(id))
        })
    }

    /// Encode this pool as a lexicographically sorted, front-coded byte blob
    /// (the linearized form of a prefix trie) and return `(encoded, remap)`,
    /// where `remap[old_id] = new_sorted_id`.
    pub fn encode_front_coded(&self) -> (Vec<u8>, Vec<NameId>) {
        let count = self.len();
        if count == 0 {
            return (Vec::new(), Vec::new());
        }

        let mut order: Vec<u32> = (0..count as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            self.get(NameId(a))
                .as_bytes()
                .cmp(self.get(NameId(b)).as_bytes())
        });

        let mut remap = vec![NameId(0); count];
        let mut encoded = Vec::with_capacity(self.data.len() / 2 + count * 2);
        let mut prev: &[u8] = &[];

        for (new_idx, &old_idx) in order.iter().enumerate() {
            remap[old_idx as usize] = NameId(new_idx as u32);
            let current = self.get(NameId(old_idx)).as_bytes();
            let prefix_len = shared_prefix_len(prev, current);
            let suffix = &current[prefix_len..];
            write_varint(prefix_len as u64, &mut encoded);
            write_varint(suffix.len() as u64, &mut encoded);
            encoded.extend_from_slice(suffix);
            prev = current;
        }

        (encoded, remap)
    }

    /// Decode a front-coded dictionary produced by [`Self::encode_front_coded`]
    /// in a single linear pass directly into a fresh [`StringPool`].
    pub fn decode_front_coded(count: u32, encoded: &[u8]) -> io::Result<Self> {
        let count = count as usize;
        if count == 0 {
            return Ok(Self::new());
        }

        let mut pool =
            Self::with_capacity(count, encoded.len().saturating_mul(2));
        let mut cursor = 0usize;
        let mut prev_start = 0usize;
        let mut prev_len = 0usize;
        let mut prefix_scratch = Vec::new();

        for _ in 0..count {
            let prefix_len = read_varint(encoded, &mut cursor)? as usize;
            let suffix_len = read_varint(encoded, &mut cursor)? as usize;
            if prefix_len > prev_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "front-coded string prefix exceeds previous string",
                ));
            }
            let suffix_end =
                cursor.checked_add(suffix_len).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "front-coded string length overflow",
                    )
                })?;
            let suffix = encoded.get(cursor..suffix_end).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated front-coded string dictionary",
                )
            })?;
            cursor = suffix_end;

            let suffix_str = std::str::from_utf8(suffix).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, error)
            })?;

            let new_start = pool.data.len();
            if prefix_len > 0 {
                let prefix_end = prev_start + prefix_len;
                if !pool.data.is_char_boundary(prefix_end) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "front-coded prefix splits a UTF-8 character",
                    ));
                }
                prefix_scratch.clear();
                prefix_scratch.extend_from_slice(
                    &pool.data.as_bytes()[prev_start..prefix_end],
                );
                let prefix_str =
                    std::str::from_utf8(&prefix_scratch).map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error)
                    })?;
                pool.data.push_str(prefix_str);
            }
            pool.data.push_str(suffix_str);

            let new_len = pool.data.len() - new_start;
            let id = NameId(pool.len() as u32);
            pool.offsets.push(pool.data.len() as u32);
            let hash = Self::hash_str(&pool.data[new_start..]);
            if let std::collections::hash_map::Entry::Vacant(e) =
                pool.index.entry(hash)
            {
                e.insert(id);
            } else {
                pool.collisions.push((hash, id));
            }

            prev_start = new_start;
            prev_len = new_len;
        }

        Ok(pool)
    }

    fn hash_str(value: &str) -> u64 {
        let mut hasher = FxBuildHasher.build_hasher();
        hasher.write(value.as_bytes());
        hasher.finish()
    }
}

/// Longest common prefix in valid UTF-8 characters (so front-coding never
/// splits a multi-byte UTF-8 sequence).
fn shared_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let mut len = a
        .iter()
        .zip(b.iter())
        .take_while(|(left, right)| left == right)
        .count();
    while len > 0 && (a[len - 1] & 0b1100_0000) == 0b1000_0000 {
        len -= 1;
    }
    if len > 0 && len < a.len() && (a[len] & 0b1100_0000) == 0b1000_0000 {
        while len > 0 && (a[len - 1] & 0b1100_0000) == 0b1000_0000 {
            len -= 1;
        }
        len = len.saturating_sub(1);
    }
    len
}

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> io::Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = bytes.get(*cursor) else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated varint in string dictionary",
            ));
        };
        *cursor += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "varint overflow in string dictionary",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_deduplicates_and_preserves_order() {
        let mut pool = StringPool::new();
        let a = pool.intern("Cargo.toml");
        let b = pool.intern("src");
        let a2 = pool.intern("Cargo.toml");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.get(a), "Cargo.toml");
        assert_eq!(pool.get(b), "src");
    }

    #[test]
    fn front_coding_roundtrips_and_compresses_shared_prefixes() {
        let mut pool = StringPool::new();
        let names = [
            "libstdc++.so.6.0.33",
            "libstdc++.so.6",
            "libstdc++.so",
            "libssl.so.3",
            "libssl.a",
            "Cargo.toml",
            "Cargo.lock",
            "café-élan",
            "café-école",
            "日本語-ファイル1",
            "日本語-ファイル2",
        ];
        let ids: Vec<NameId> = names.iter().map(|n| pool.intern(n)).collect();

        let (encoded, remap) = pool.encode_front_coded();
        assert!(
            encoded.len() < pool.byte_len(),
            "expected front-coding to save space: {} vs {}",
            encoded.len(),
            pool.byte_len()
        );

        let decoded =
            StringPool::decode_front_coded(pool.len() as u32, &encoded)
                .expect("decode");
        assert_eq!(decoded.len(), pool.len());
        for (name, old_id) in names.iter().zip(ids) {
            let new_id = remap[old_id.index()];
            assert_eq!(decoded.get(new_id), *name);
        }
    }
}
