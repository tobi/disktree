//! Hand-written Protobuf schema (`prost::Message`), columnar `SubtreeChunk`
//! encoding/decoding, and LZ4 frame compression.
//!
//! Mirrors `proto/disktree_cache.proto` directly with zero `protoc` or
//! `build.rs` dependency.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::io;
use std::path::PathBuf;

use rustc_hash::FxHashMap;

use crate::strings::{NameId, StringPool};
use crate::tree::{ArenaNode, HardlinkEntry, NodeFlags, NodeId, TreeArena};
use crate::types::SubtreeStatus;

/// Current binary cache schema version.
pub const FORMAT_VERSION: u32 = 1;

/// Frame codec tags prepended to stored chunks.
pub const CODEC_RAW: u8 = 0;
pub const CODEC_LZ4: u8 = 1;

/// Directory coverage states stored in [`DirIndexEntry::coverage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Coverage {
    /// Ancestor placeholder; directory itself has not been scanned.
    Stub = 0,
    /// Full recursive scan completed under its recorded options.
    Complete = 1,
    /// Scanned only down to `scanned_max_depth`.
    Shallow = 2,
    /// Stopped at a mount boundary.
    MountBoundary = 3,
    /// Directory could not be read.
    ReadError = 4,
}

impl Coverage {
    pub const fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Complete,
            2 => Self::Shallow,
            3 => Self::MountBoundary,
            4 => Self::ReadError,
            _ => Self::Stub,
        }
    }
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct DbHeader {
    #[prost(uint32, tag = "1")]
    pub format_version: u32,
    #[prost(uint64, tag = "2")]
    pub created_at_unix: u64,
    #[prost(uint32, tag = "3")]
    pub codec: u32,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct HostRecord {
    #[prost(string, tag = "1")]
    pub host_key: String,
    #[prost(string, tag = "2")]
    pub hostname: String,
    #[prost(string, tag = "3")]
    pub machine_id: String,
    #[prost(string, tag = "4")]
    pub platform: String,
    #[prost(uint32, tag = "5")]
    pub path_style: u32,
    #[prost(uint64, tag = "6")]
    pub updated_at_unix: u64,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct DirIndexEntry {
    #[prost(uint64, tag = "1")]
    pub dir_id: u64,
    #[prost(uint64, tag = "2")]
    pub parent_id: u64,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(uint64, tag = "4")]
    pub scanned_at_unix: u64,
    #[prost(uint32, tag = "5")]
    pub coverage: u32,
    #[prost(uint32, tag = "6")]
    pub scanned_max_depth: u32,
    #[prost(uint32, tag = "7")]
    pub options_mask: u32,
    #[prost(uint64, tag = "8")]
    pub mount_dev: u64,
    #[prost(int64, tag = "9")]
    pub dir_mtime: i64,
    #[prost(uint64, tag = "10")]
    pub chunk_id: u64,
    #[prost(uint64, tag = "11")]
    pub allocated_bytes: u64,
    #[prost(uint64, tag = "12")]
    pub apparent_bytes: u64,
    #[prost(uint64, tag = "13")]
    pub files: u64,
    #[prost(uint32, tag = "14")]
    pub dirs: u32,
    #[prost(int64, tag = "15")]
    pub newest_mtime: i64,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FrontCodedStrings {
    #[prost(uint32, tag = "1")]
    pub count: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub encoded: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct SubtreeChunk {
    #[prost(uint64, tag = "1")]
    pub chunk_id: u64,
    #[prost(uint64, tag = "2")]
    pub root_dir_id: u64,
    #[prost(uint64, tag = "3")]
    pub scanned_at_unix: u64,
    #[prost(int64, tag = "4")]
    pub base_mtime: i64,
    #[prost(message, optional, tag = "5")]
    pub strings: Option<FrontCodedStrings>,

    #[prost(uint32, repeated, packed = "true", tag = "6")]
    pub name_ids: Vec<u32>,
    #[prost(uint32, repeated, packed = "true", tag = "7")]
    pub child_counts: Vec<u32>,
    #[prost(uint32, repeated, packed = "true", tag = "8")]
    pub flags: Vec<u32>,
    #[prost(uint64, repeated, packed = "true", tag = "9")]
    pub allocated_bytes: Vec<u64>,
    #[prost(sint64, repeated, packed = "true", tag = "10")]
    pub apparent_deltas: Vec<i64>,
    #[prost(sint64, repeated, packed = "true", tag = "11")]
    pub mtime_deltas: Vec<i64>,

    #[prost(uint32, repeated, packed = "true", tag = "12")]
    pub hardlink_node_idx: Vec<u32>,
    #[prost(uint64, repeated, packed = "true", tag = "13")]
    pub hardlink_devs: Vec<u64>,
    #[prost(uint64, repeated, packed = "true", tag = "14")]
    pub hardlink_inos: Vec<u64>,

    #[prost(uint64, repeated, packed = "true", tag = "15")]
    pub external_dir_ids: Vec<u64>,
}

/// Decoded chunk together with any external child shard links that must be
/// grafted into the returned arena.
#[derive(Debug)]
pub struct DecodedChunk {
    pub arena: TreeArena,
    pub scanned_at: u64,
    /// `(node_id_in_arena, child_dir_id_in_db)`
    pub external_links: Vec<(NodeId, u64)>,
}

/// Encode a protobuf message into a 1-byte codec-tagged frame, using LZ4 when
/// it saves space.
pub fn encode_framed<M: prost::Message>(msg: &M) -> Vec<u8> {
    let raw = msg.encode_to_vec();
    if raw.len() >= 64 {
        let compressed = lz4_flex::compress_prepend_size(&raw);
        if compressed.len() < raw.len() {
            let mut framed = Vec::with_capacity(1 + compressed.len());
            framed.push(CODEC_LZ4);
            framed.extend_from_slice(&compressed);
            return framed;
        }
    }
    let mut framed = Vec::with_capacity(1 + raw.len());
    framed.push(CODEC_RAW);
    framed.extend_from_slice(&raw);
    framed
}

/// Decode a codec-tagged protobuf frame produced by [`encode_framed`].
pub fn decode_framed<M: prost::Message + Default>(
    bytes: &[u8],
) -> io::Result<M> {
    let (&codec, payload) = bytes.split_first().ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "empty protobuf frame")
    })?;
    match codec {
        CODEC_RAW => M::decode(payload)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        CODEC_LZ4 => {
            let raw = lz4_flex::decompress_size_prepended(payload).map_err(
                |err| io::Error::new(io::ErrorKind::InvalidData, err),
            )?;
            M::decode(raw.as_slice())
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported protobuf frame codec {other}"),
        )),
    }
}

/// Encode a subtree of `arena` rooted at `sub_root` into a columnar
/// [`SubtreeChunk`], stopping at any descendant listed in `external_shards`.
pub fn encode_subtree_chunk<S: BuildHasher>(
    arena: &TreeArena,
    sub_root: NodeId,
    chunk_id: u64,
    root_dir_id: u64,
    scanned_at_unix: u64,
    external_shards: &HashMap<NodeId, u64, S>,
) -> SubtreeChunk {
    // Index hardlinks by node for O(1) lookup during preorder walk.
    let mut hardlink_by_node: FxHashMap<NodeId, Vec<(u64, u64)>> =
        FxHashMap::default();
    for link in arena.hardlinks() {
        hardlink_by_node
            .entry(link.node)
            .or_default()
            .push((link.dev, link.ino));
    }

    let mut local_strings = StringPool::new();
    let mut name_ids = Vec::new();
    let mut child_counts = Vec::new();
    let mut flags = Vec::new();
    let mut allocated_bytes = Vec::new();
    let mut apparent_deltas = Vec::new();
    let mut mtime_deltas = Vec::new();
    let mut hardlink_node_idx = Vec::new();
    let mut hardlink_devs = Vec::new();
    let mut hardlink_inos = Vec::new();
    let mut external_dir_ids = Vec::new();

    let base_mtime = arena.node(sub_root).modified;
    let mut prev_mtime = base_mtime;

    let mut stack = vec![sub_root];
    while let Some(node_id) = stack.pop() {
        let node = arena.node(node_id);
        let preorder_idx = name_ids.len() as u32;
        let local_name = local_strings.intern(arena.name(node_id));
        name_ids.push(local_name.0);

        let is_external =
            node_id != sub_root && external_shards.contains_key(&node_id);

        let mut bits = node.flags.persistent_bits();
        if is_external {
            bits |= NodeFlags::EXTERNAL_SHARD_REF;
            flags.push(bits);
            child_counts.push(0);
            allocated_bytes.push(0);
            apparent_deltas.push(0);
            mtime_deltas.push(node.modified.saturating_sub(prev_mtime));
            prev_mtime = node.modified;
            external_dir_ids.push(external_shards[&node_id]);
            continue;
        }

        flags.push(bits);

        if node.is_dir() {
            let kids: Vec<NodeId> = arena.children(node_id).collect();
            child_counts.push(kids.len() as u32);
            // Derived totals are recomputed on decode; store 0 for dirs to
            // maximize compression and uphold Invariant #2.
            allocated_bytes.push(0);
            apparent_deltas.push(0);
            mtime_deltas.push(node.modified.saturating_sub(prev_mtime));
            prev_mtime = node.modified;
            for kid in kids.into_iter().rev() {
                stack.push(kid);
            }
        } else {
            child_counts.push(0);
            allocated_bytes.push(node.allocated_bytes);
            let delta = i128::from(node.apparent_bytes)
                - i128::from(node.allocated_bytes);
            let clamped =
                delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
            apparent_deltas.push(clamped);
            mtime_deltas.push(node.modified.saturating_sub(prev_mtime));
            prev_mtime = node.modified;

            if let Some(links) = hardlink_by_node.get(&node_id) {
                for &(dev, ino) in links {
                    hardlink_node_idx.push(preorder_idx);
                    hardlink_devs.push(dev);
                    hardlink_inos.push(ino);
                }
            }
        }
    }

    // Prefix-trie front-code the local string dictionary and remap name IDs.
    let (encoded_strings, remap) = local_strings.encode_front_coded();
    for id in &mut name_ids {
        *id = remap[*id as usize].0;
    }

    SubtreeChunk {
        chunk_id,
        root_dir_id,
        scanned_at_unix,
        base_mtime,
        strings: Some(FrontCodedStrings {
            count: local_strings.len() as u32,
            encoded: encoded_strings,
        }),
        name_ids,
        child_counts,
        flags,
        allocated_bytes,
        apparent_deltas,
        mtime_deltas,
        hardlink_node_idx,
        hardlink_devs,
        hardlink_inos,
        external_dir_ids,
    }
}

/// Decode a columnar [`SubtreeChunk`] back into a [`TreeArena`] and its
/// external child shard references.
pub fn decode_subtree_chunk(
    chunk: &SubtreeChunk,
    root_path: PathBuf,
) -> io::Result<DecodedChunk> {
    let count = chunk.name_ids.len();
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty subtree chunk",
        ));
    }
    if chunk.child_counts.len() != count
        || chunk.flags.len() != count
        || chunk.allocated_bytes.len() != count
        || chunk.apparent_deltas.len() != count
        || chunk.mtime_deltas.len() != count
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mismatched columnar lengths in subtree chunk",
        ));
    }
    if chunk.hardlink_node_idx.len() != chunk.hardlink_devs.len()
        || chunk.hardlink_node_idx.len() != chunk.hardlink_inos.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mismatched hardlink arrays in subtree chunk",
        ));
    }

    let strings = match &chunk.strings {
        Some(dict) => {
            StringPool::decode_front_coded(dict.count, &dict.encoded)?
        }
        None => StringPool::new(),
    };

    let mut nodes = Vec::with_capacity(count);
    let mut last_child: Vec<Option<NodeId>> = vec![None; count];
    let mut external_links = Vec::with_capacity(chunk.external_dir_ids.len());
    let mut ext_cursor = 0usize;
    let mut cur_mtime = chunk.base_mtime;

    // Stack of `(parent_node_id, remaining_children_expected)`.
    let mut parent_stack: Vec<(NodeId, u32)> = Vec::new();

    for idx in 0..count {
        let name_raw = chunk.name_ids[idx];
        if (name_raw as usize) >= strings.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "name_id out of bounds in subtree chunk",
            ));
        }

        let parent = if idx == 0 {
            None
        } else {
            let Some(&(parent_id, _)) = parent_stack.last() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "orphan node in subtree chunk preorder",
                ));
            };
            Some(parent_id)
        };

        if let Some((_, remaining)) = parent_stack.last_mut() {
            *remaining -= 1;
            if *remaining == 0 {
                parent_stack.pop();
            }
        }

        let raw_flags = chunk.flags[idx];
        let mut node_flags = NodeFlags(raw_flags);
        let is_external = node_flags.has(NodeFlags::EXTERNAL_SHARD_REF);
        node_flags.set(NodeFlags::EXTERNAL_SHARD_REF, false);

        cur_mtime = cur_mtime.saturating_add(chunk.mtime_deltas[idx]);
        let alloc = chunk.allocated_bytes[idx];
        let apparent = if node_flags.kind().is_dir() {
            0
        } else {
            let sum =
                i128::from(alloc) + i128::from(chunk.apparent_deltas[idx]);
            sum.max(0) as u64
        };

        let is_file = node_flags.kind() == crate::types::NodeKind::File;
        let is_dir = node_flags.kind().is_dir();

        let id = NodeId::from_index(idx).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "node index overflow")
        })?;

        let mut node = ArenaNode {
            name: NameId(name_raw),
            parent,
            first_child: None,
            next_sibling: None,
            allocated_bytes: alloc,
            apparent_bytes: apparent,
            files: u64::from(is_file),
            dirs: u32::from(is_dir),
            flags: node_flags,
            modified: cur_mtime,
            scanned_at: if is_dir { chunk.scanned_at_unix } else { 0 },
        };
        if is_dir && chunk.scanned_at_unix > 0 {
            node.set_status(SubtreeStatus::Fresh {
                scanned_at: chunk.scanned_at_unix,
            });
        }
        nodes.push(node);

        if let Some(parent_id) = parent {
            let p = parent_id.index();
            if let Some(prev) = last_child[p] {
                nodes[prev.index()].next_sibling = Some(id);
            } else {
                nodes[p].first_child = Some(id);
            }
            last_child[p] = Some(id);
        }

        if is_external {
            let Some(&ext_dir_id) = chunk.external_dir_ids.get(ext_cursor)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing external_dir_id for shard ref",
                ));
            };
            ext_cursor += 1;
            external_links.push((id, ext_dir_id));
        } else {
            let child_count = chunk.child_counts[idx];
            if child_count > 0 {
                parent_stack.push((id, child_count));
            }
        }
    }

    if !parent_stack.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated children in subtree chunk",
        ));
    }

    let mut hardlinks = Vec::with_capacity(chunk.hardlink_node_idx.len());
    for ((&node_idx, &dev), &ino) in chunk
        .hardlink_node_idx
        .iter()
        .zip(&chunk.hardlink_devs)
        .zip(&chunk.hardlink_inos)
    {
        if (node_idx as usize) >= count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hardlink node index out of bounds",
            ));
        }
        let node = NodeId::from_index(node_idx as usize).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "hardlink id overflow")
        })?;
        hardlinks.push(HardlinkEntry { node, dev, ino });
    }

    let arena =
        TreeArena::from_parts(root_path, nodes, last_child, strings, hardlinks);

    Ok(DecodedChunk {
        arena,
        scanned_at: chunk.scanned_at_unix,
        external_links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeKind;

    #[test]
    fn subtree_chunk_roundtrips_columnar_data_and_hardlinks() {
        let mut arena = TreeArena::new(PathBuf::from("/data"), "data");
        let sub = arena.add_child(
            arena.root(),
            "crates",
            NodeKind::Directory,
            0,
            0,
            1_700_000_000,
            NodeFlags::new(NodeKind::Directory),
        );
        let f1 = arena.add_child(
            sub,
            "Cargo.toml",
            NodeKind::File,
            4096,
            1234,
            1_700_000_100,
            NodeFlags::new(NodeKind::File),
        );
        let f2 = arena.add_child(
            sub,
            "Cargo.lock",
            NodeKind::File,
            4096,
            1234,
            1_700_000_200,
            NodeFlags::new(NodeKind::File),
        );
        arena.record_hardlink(f1, 42, 777);
        arena.record_hardlink(f2, 42, 777);

        let chunk = encode_subtree_chunk(
            &arena,
            arena.root(),
            1,
            10,
            1_700_001_000,
            &FxHashMap::default(),
        );
        let framed = encode_framed(&chunk);
        let decoded_proto: SubtreeChunk =
            decode_framed(&framed).expect("decode frame");
        let mut decoded =
            decode_subtree_chunk(&decoded_proto, PathBuf::from("/data"))
                .expect("decode chunk");
        decoded.arena.aggregate(true, true);

        assert_eq!(decoded.scanned_at, 1_700_001_000);
        assert_eq!(decoded.arena.len(), 4);
        assert_eq!(
            decoded.arena.node(decoded.arena.root()).allocated_bytes,
            4096
        );
        assert_eq!(
            decoded.arena.node(decoded.arena.root()).apparent_bytes,
            1234
        );
        assert_eq!(decoded.arena.node(decoded.arena.root()).files, 2);
        assert_eq!(decoded.arena.hardlinks().len(), 2);
    }

    #[test]
    fn external_shard_refs_stop_traversal_and_decode_links() {
        let mut arena = TreeArena::new(PathBuf::from("/root"), "root");
        let big = arena.add_child(
            arena.root(),
            "big",
            NodeKind::Directory,
            0,
            0,
            100,
            NodeFlags::new(NodeKind::Directory),
        );
        let _inside = arena.add_child(
            big,
            "hidden_inside_shard.bin",
            NodeKind::File,
            8192,
            8000,
            100,
            NodeFlags::new(NodeKind::File),
        );
        let _sibling = arena.add_child(
            arena.root(),
            "small.txt",
            NodeKind::File,
            4096,
            12,
            100,
            NodeFlags::new(NodeKind::File),
        );

        let mut ext = FxHashMap::default();
        ext.insert(big, 999);

        let chunk =
            encode_subtree_chunk(&arena, arena.root(), 1, 1, 5000, &ext);
        let decoded = decode_subtree_chunk(&chunk, PathBuf::from("/root"))
            .expect("decode");
        // Only root, big (stub ref), and small.txt should be in this chunk.
        assert_eq!(decoded.arena.len(), 3);
        assert_eq!(decoded.external_links.len(), 1);
        assert_eq!(decoded.external_links[0].1, 999);
        assert_eq!(decoded.arena.name(decoded.external_links[0].0), "big");
    }
}
