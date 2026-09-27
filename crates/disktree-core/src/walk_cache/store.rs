//! A finished walk of one volume kept on disk: a header, the root path,
//! the files last seen open, then the tree a directory at a time, every
//! directory's entries after those of the directories above it, so each
//! run reads back into place. A leaf keeps its size, its count following
//! from its kind; a directory keeps its totals, so nothing is summed again
//! on load. Identities keep only the file number, their device being the
//! state's volume. The file is written and read as it streams, a megabyte
//! at a time, never whole in memory. Anything that does not check out
//! (another format, a torn or damaged file, a count or name that cannot
//! be) loads as nothing, and the caller walks again.

use std::hash::Hasher as _;
use std::io::{self, Read, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHasher;

use crate::tree::{
    DIRECTORY, Dir, IDENTIFIED, Item, READ_ERROR, Seg, Tree, code,
    decode as kind_of,
};

/// Bumped for layout or scan-policy changes, including incomplete listings.
/// Older snapshots must not hide an error the newer walk would retry.
const MAGIC: [u8; 8] = *b"dtwalk\x00\x04";

/// Largest file kept or read, header included.
const MOST_BYTES: usize = 1 << 30;
const MOST_NODES: u64 = 10_000_000;
/// Deepest node below the root.
const MOST_DEPTH: usize = 512;

/// Magic, checksum of everything after it, then volume, root id, journal,
/// next, created, options, and the counts of open files, root bytes,
/// nodes, directories and name bytes.
const HEADER: usize = 8 * 13;
#[cfg(test)]
const NODE_COUNT_AT: usize = 80;

/// Fewest bytes a node takes: tag, a one-byte name and its length,
/// modified and size.
const LEAST_RECORD: usize = 5;

/// Bytes hashed at a time: see [`checksum`].
const PART: usize = 1 << 20;

/// Tag bits: the kind, then flags. Anything above `KNOWN` is damage.
const KIND: u8 = 0b11;
const BROKEN: u8 = 1 << 2;
/// A file number follows; its device is the state's volume.
const INODE: u8 = 1 << 3;
const KNOWN: u8 = KIND | BROKEN | INODE;

/// What the next scan starts from.
#[derive(Debug)]
pub(super) struct State {
    pub root: String,
    pub volume: u64,
    pub root_id: u64,
    pub journal: u64,
    pub next: u64,
    /// Unix seconds.
    pub created: u64,
    pub options: u64,
    pub open: Vec<u64>,
    /// Shared with the scan that handed it over: kept without a copy.
    pub tree: Arc<Tree>,
}

/// The state kept in `path`, if it is whole and of this format.
pub(super) fn load(path: &Path) -> Option<State> {
    let file = crate::windows::cache_read(path)?;
    let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
    decode(file, len)
}

/// Write `state` to `path`, whole or not at all: a failed or interrupted
/// save leaves whatever was there before. Not synced: a crash that loses
/// the new bytes fails the checksum, and the next scan walks again.
pub(super) fn save(path: &Path, state: &State) -> io::Result<()> {
    crate::windows::cache_write(path, |out| {
        let sum = encode(state, &mut *out)?;
        out.seek(SeekFrom::Start(8))?;
        out.write_all(&sum.to_le_bytes())
    })
}

/// A check a torn or damaged file fails: the length of everything past
/// the checksum, then a hash of each megabyte of it in turn.
fn checksum(len: usize, parts: &[u64]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write_usize(len);
    for &part in parts {
        hasher.write_u64(part);
    }
    hasher.finish()
}

fn part(bytes: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

/// Bytes written out a part at a time, each part hashed on the way; the
/// first sixteen, the magic and the checksum's place, are not.
struct Sealed<W: Write> {
    out: W,
    part: Vec<u8>,
    parts: Vec<u64>,
    /// Bytes hashed so far.
    len: usize,
    /// Bytes still to pass before hashing starts.
    head: usize,
}

impl<W: Write> Sealed<W> {
    fn new(out: W) -> Self {
        Self {
            out,
            part: Vec::with_capacity(PART),
            parts: Vec::new(),
            len: 0,
            head: 16,
        }
    }

    fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self.head > 0 {
            let (head, rest) = bytes.split_at(self.head.min(bytes.len()));
            self.out.write_all(head)?;
            self.head -= head.len();
            bytes = rest;
        }
        self.len += bytes.len();
        if self.len + HEADER > MOST_BYTES {
            return Err(too_large());
        }
        while !bytes.is_empty() {
            let room = PART - self.part.len();
            let (now, later) = bytes.split_at(room.min(bytes.len()));
            self.part.extend_from_slice(now);
            bytes = later;
            if self.part.len() == PART {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.parts.push(part(&self.part));
        self.out.write_all(&self.part)?;
        self.part.clear();
        Ok(())
    }

    fn u64(&mut self, value: u64) -> io::Result<()> {
        self.write(&value.to_le_bytes())
    }

    fn varint(&mut self, mut value: u64) -> io::Result<()> {
        let mut bytes = [0; 10];
        let mut len = 0;
        while value >= 0x80 {
            bytes[len] = value as u8 | 0x80;
            value >>= 7;
            len += 1;
        }
        bytes[len] = value as u8;
        self.write(&bytes[..=len])
    }

    /// The checksum of everything written.
    fn finish(mut self) -> io::Result<u64> {
        if !self.part.is_empty() {
            self.flush()?;
        }
        self.out.flush()?;
        Ok(checksum(self.len, &self.parts))
    }
}

/// Write `state` to `out`, the checksum's place left zero; returns the
/// checksum, which belongs at byte 8.
fn encode(state: &State, out: impl Write) -> io::Result<u64> {
    if state.root.contains('\0') {
        return Err(invalid("root path holds a NUL"));
    }
    let tree = &state.tree;
    // Every directory in the order its entries are written, with its
    // depth: counted first, as the header gives the counts.
    let mut order: Vec<(u32, usize)> = vec![(0, 0)];
    let mut nodes: u64 = 1;
    // Names below the root: the root's is kept apart from them.
    let mut text: u64 = 0;
    let mut at = 0;
    while let Some(&(index, depth)) = order.get(at) {
        let dir = tree
            .dirs
            .get(index as usize)
            .ok_or_else(|| invalid("no such directory"))?;
        let run = tree.run(dir);
        if !run.is_empty() && depth >= MOST_DEPTH {
            return Err(too_large());
        }
        for item in run {
            nodes += 1;
            text += u64::from(item.len);
            if item.kind == DIRECTORY {
                order.push((item.value as u32, depth + 1));
            }
        }
        // A tree that loops never stops growing past this.
        if nodes > MOST_NODES {
            return Err(too_large());
        }
        at += 1;
    }
    let mut out = Sealed::new(out);
    out.write(&MAGIC)?;
    for value in [
        0,
        state.volume,
        state.root_id,
        state.journal,
        state.next,
        state.created,
        state.options,
        state.open.len() as u64,
        state.root.len() as u64,
        nodes,
        order.len() as u64,
        text,
    ] {
        out.u64(value)?;
    }
    out.write(state.root.as_bytes())?;
    for &number in &state.open {
        out.u64(number)?;
    }
    let root = tree.dirs.first().copied().unwrap_or(Dir::EMPTY);
    write_dir(&mut out, state, &tree.name, &root, 0)?;
    for &(index, depth) in &order {
        let Some(dir) = tree.dirs.get(index as usize) else {
            continue;
        };
        for item in tree.run(dir) {
            let name = tree.text(dir.seg, item);
            if item.kind == DIRECTORY {
                let child = tree
                    .dir(item.value)
                    .ok_or_else(|| invalid("no such directory"))?;
                write_dir(&mut out, state, name, child, depth + 1)?;
            } else {
                write_leaf(&mut out, state, name, item, depth + 1)?;
            }
        }
    }
    out.finish()
}

/// `id`, whose device is `volume` in the tree, as the file keeps it.
fn identity(state: &State, volume: u16, id: u64) -> io::Result<u64> {
    if state.tree.volume(volume) == state.volume {
        Ok(id)
    } else {
        Err(invalid("an identity of another volume"))
    }
}

/// A leaf: tag, modified, identity, size, then its name, last so a
/// reader takes it straight from what it read in.
fn write_leaf<W: Write>(
    out: &mut Sealed<W>,
    state: &State,
    name: &str,
    item: &Item,
    depth: usize,
) -> io::Result<()> {
    let inode = item.identified().then_some(item.id);
    out.write(&[item.kind | if inode.is_some() { INODE } else { 0 }])?;
    out.varint(u64::from(item.modified))?;
    if let Some(inode) = inode {
        out.varint(identity(state, item.volume, inode)?)?;
    }
    out.varint(item.value)?;
    write_name(out, name, depth)
}

/// A directory: tag, kinds, modified, identity, totals and count of
/// entries, then its name.
fn write_dir<W: Write>(
    out: &mut Sealed<W>,
    state: &State,
    name: &str,
    dir: &Dir,
    depth: usize,
) -> io::Result<()> {
    let inode = dir.identified().then_some(dir.id);
    let tag = DIRECTORY
        | if dir.flags & READ_ERROR != 0 {
            BROKEN
        } else {
            0
        }
        | if inode.is_some() { INODE } else { 0 };
    // An undecided kind is kept as none.
    let (category, reclaim) = kind_of((dir.category, dir.reclaim));
    let (category, reclaim) = code(category, reclaim);
    out.write(&[tag, category | reclaim << 4])?;
    out.varint(u64::from(dir.modified))?;
    if let Some(inode) = inode {
        out.varint(identity(state, dir.volume, inode)?)?;
    }
    out.varint(dir.bytes)?;
    out.varint(dir.files)?;
    out.varint(u64::from(dir.dirs))?;
    out.varint(u64::from(dir.len))?;
    write_name(out, name, depth)
}

fn write_name<W: Write>(
    out: &mut Sealed<W>,
    name: &str,
    depth: usize,
) -> io::Result<()> {
    if !name_fits(name, depth) {
        return Err(invalid("a name cannot be kept"));
    }
    out.varint(name.len() as u64)?;
    out.write(name.as_bytes())
}

/// Bytes read in as they are needed, a part at a time, each part hashed
/// as it comes in.
struct Stream<R: Read> {
    input: R,
    block: Vec<u8>,
    at: usize,
    /// Hashed bytes not read in yet.
    left: usize,
    /// All the hashed bytes.
    len: usize,
    parts: Vec<u64>,
    /// A name that crosses from one part into the next, put together.
    spill: Vec<u8>,
}

impl<R: Read> Stream<R> {
    fn refill(&mut self) -> Option<()> {
        let size = PART.min(self.left);
        if size == 0 {
            return None;
        }
        self.block.resize(size, 0);
        self.input.read_exact(&mut self.block).ok()?;
        self.parts.push(part(&self.block));
        self.left -= size;
        self.at = 0;
        Some(())
    }

    fn byte(&mut self) -> Option<u8> {
        if self.at == self.block.len() {
            self.refill()?;
        }
        let byte = self.block[self.at];
        self.at += 1;
        Some(byte)
    }

    fn take(&mut self, len: usize) -> Option<&[u8]> {
        if self.block.len() - self.at >= len {
            let bytes = &self.block[self.at..self.at + len];
            self.at += len;
            return Some(bytes);
        }
        self.spill.clear();
        while self.spill.len() < len {
            if self.at == self.block.len() {
                self.refill()?;
            }
            let count =
                (len - self.spill.len()).min(self.block.len() - self.at);
            self.spill
                .extend_from_slice(&self.block[self.at..self.at + count]);
            self.at += count;
        }
        Some(&self.spill)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0;
        for shift in (0_u32..64).step_by(7) {
            let byte = self.byte()?;
            let part = u64::from(byte & 0x7F);
            if shift == 63 && part > 1 {
                return None;
            }
            value |= part << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    /// Whether everything was read, and it is what was written.
    fn whole(&self, sum: u64) -> bool {
        self.at == self.block.len()
            && self.left == 0
            && checksum(self.len, &self.parts) == sum
    }
}

/// One node as the file keeps it.
struct Record<'a> {
    name: &'a str,
    item: Item,
    /// A directory's own facts, and how many entries it holds.
    dir: Option<(Dir, usize)>,
}

impl<R: Read> Stream<R> {
    fn record(&mut self, depth: usize) -> Option<Record<'_>> {
        let tag = self.byte()?;
        if tag & !KNOWN != 0 {
            return None;
        }
        let kind = tag & KIND;
        let classes = if kind == DIRECTORY {
            let classes = self.byte()?;
            (category_from(classes & 0x0F)?, reclaim_from(classes >> 4)?)
        } else if tag & BROKEN != 0 {
            return None;
        } else {
            (0, 0)
        };
        let modified = u32::try_from(self.varint()?).ok()?;
        let inode = if tag & INODE == 0 {
            None
        } else {
            Some(self.varint()?)
        };
        let mut item = Item {
            modified,
            kind,
            ..Item::default()
        };
        if let Some(id) = inode {
            item.id = id;
            item.flags = IDENTIFIED;
        }
        let dir = if kind == DIRECTORY {
            let mut dir = Dir {
                bytes: self.varint()?,
                files: self.varint()?,
                dirs: u32::try_from(self.varint()?).ok()?,
                modified,
                category: classes.0,
                reclaim: classes.1,
                flags: if tag & BROKEN == 0 { 0 } else { READ_ERROR },
                ..Dir::EMPTY
            };
            if let Some(id) = inode {
                dir.id = id;
                dir.flags |= IDENTIFIED;
            }
            Some((dir, usize::try_from(self.varint()?).ok()?))
        } else {
            item.value = self.varint()?;
            None
        };
        let len = usize::try_from(self.varint()?).ok()?;
        // An entry keeps its name's length in 16 bits; only the root's is
        // the tree's own. No file system names a file that long.
        if depth > 0 && len > usize::from(u16::MAX) {
            return None;
        }
        let name = std::str::from_utf8(self.take(len)?).ok()?;
        if !name_fits(name, depth) {
            return None;
        }
        Some(Record { name, item, dir })
    }
}

fn decode(input: impl Read, len: usize) -> Option<State> {
    if !(HEADER..=MOST_BYTES).contains(&len) {
        return None;
    }
    let mut stream = Stream {
        input,
        block: Vec::with_capacity(PART.min(len)),
        at: 0,
        left: len - 16,
        len: len - 16,
        parts: Vec::new(),
        spill: Vec::new(),
    };
    let mut head = [0_u8; 16];
    stream.input.read_exact(&mut head).ok()?;
    if head[..8] != MAGIC {
        return None;
    }
    let sum = u64::from_le_bytes(head[8..].try_into().ok()?);
    let volume = stream.u64()?;
    let root_id = stream.u64()?;
    let journal = stream.u64()?;
    let next = stream.u64()?;
    let created = stream.u64()?;
    let options = stream.u64()?;
    let open = stream.count()?;
    let root = stream.count()?;
    let nodes = stream.u64()?;
    let dirs = stream.count()?;
    let text = stream.count()?;
    // Bounded before anything is sized by them: a count no byte of the
    // file could fill is damage.
    if nodes == 0
        || nodes > MOST_NODES
        || nodes as usize > len / LEAST_RECORD
        || dirs as u64 > nodes
        || text > len
        || open > len / 8
    {
        return None;
    }
    let root = std::str::from_utf8(stream.take(root)?).ok()?.to_owned();
    if root.contains('\0') {
        return None;
    }
    let open = (0..open)
        .map(|_| stream.u64())
        .collect::<Option<Vec<u64>>>()?;
    let mut tree = Tree {
        name: Box::default(),
        dirs: Vec::with_capacity(dirs),
        segs: vec![Seg {
            items: Vec::with_capacity(nodes as usize - 1),
            text: String::with_capacity(text),
        }],
        volumes: vec![volume],
    };
    // Each directory's count of entries and depth, as it comes in.
    let mut shape: Vec<(usize, usize)> = Vec::with_capacity(dirs);
    let Record { name, dir, .. } = stream.record(0)?;
    let (root_dir, count) = dir?;
    tree.name = name.into();
    tree.dirs.push(root_dir);
    shape.push((count, 0));
    let mut nodes_left = nodes - 1;
    let mut at = 0;
    while let Some(&(count, depth)) = shape.get(at) {
        if (count > 0 && depth >= MOST_DEPTH) || count as u64 > nodes_left {
            return None;
        }
        nodes_left -= count as u64;
        let first = tree.segs[0].items.len() as u32;
        for _ in 0..count {
            let Record {
                name,
                mut item,
                dir,
            } = stream.record(depth + 1)?;
            if let Some((mut dir, count)) = dir {
                item.value = tree.dirs.len() as u64;
                dir.parent = at as u32;
                tree.dirs.push(dir);
                shape.push((count, depth + 1));
            }
            let seg = &mut tree.segs[0];
            item.at = seg.text.len() as u32;
            item.len = name.len() as u16;
            seg.text.push_str(name);
            seg.items.push(item);
        }
        let dir = &mut tree.dirs[at];
        dir.first = first;
        dir.len = count as u32;
        at += 1;
    }
    let seg = &tree.segs[0];
    if nodes_left != 0
        || tree.dirs.len() != dirs
        || seg.text.len() != text
        || !stream.whole(sum)
    {
        return None;
    }
    Some(State {
        root,
        volume,
        root_id,
        journal,
        next,
        created,
        options,
        open,
        tree: Arc::new(tree),
    })
}

/// A name below the root is one path component, with no `:` that Windows
/// could take for a drive or a stream; the root's is whatever the scan
/// called it, a drive or a path.
fn name_fits(name: &str, depth: usize) -> bool {
    if depth == 0 {
        return !name.contains('\0');
    }
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.bytes().any(|byte| {
            byte == 0 || byte == b':' || std::path::is_separator(byte as char)
        })
}

/// A category as [`code`] numbers it: the legend's, and `Other` after.
const fn category_from(code: u8) -> Option<u8> {
    if code as usize <= crate::classify::Category::LEGEND.len() {
        Some(code)
    } else {
        None
    }
}

/// A reason as [`code`] numbers it: `0` for none.
const fn reclaim_from(code: u8) -> Option<u8> {
    if code as usize <= crate::classify::Reclaim::ALL.len() {
        Some(code)
    } else {
        None
    }
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn too_large() -> io::Error {
    io::Error::other("too large to keep")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Category, Reclaim};
    use crate::tree::{Draft, Metric, Node, NodeKind};

    const VOLUME: u64 = 0xDEAD_BEEF;

    fn leaf(name: &str, kind: NodeKind, bytes: u64) -> Draft {
        Draft::entry(name, kind, bytes)
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        Draft {
            children,
            ..Draft::directory(name)
        }
    }

    /// A finished tree of one volume with every kind, flag and class.
    fn sample() -> Tree {
        let mut file = leaf("a.txt", NodeKind::File, 1234);
        file.inode = Some((VOLUME, 0xFFFF_0000_0000_1234));
        file.modified = 5;
        // A second name of a file already charged: no weight of its own.
        let mut link = leaf("hard", NodeKind::File, 0);
        link.inode = Some((VOLUME, 0xFFFF_0000_0000_1234));
        let mut odd = leaf("ödd ✓", NodeKind::File, 10);
        odd.modified = i64::from(u32::MAX);
        let mut sub = dir(
            "src",
            vec![
                leaf("link", NodeKind::Symlink, 0),
                leaf("fifo", NodeKind::Other, 0),
                Draft::directory("empty"),
            ],
        );
        sub.read_error = true;
        sub.inode = Some((VOLUME, 42));
        sub.category = Category::Code;
        sub.reclaim = Some(Reclaim::Temporary);
        let mut root = dir("C:\\", vec![sub, file, link, odd]);
        root.reclaim = Some(Reclaim::Regenerable);
        root.inode = Some((VOLUME, 5));
        Tree::from_draft(root, Metric::Bytes)
    }

    fn state(tree: Tree) -> State {
        State {
            root: "C:\\Users\\me".to_owned(),
            volume: VOLUME,
            root_id: 5,
            journal: u64::MAX,
            next: 1 << 40,
            created: 1_790_000_000,
            options: 0b101,
            open: vec![1, u64::MAX, 3],
            tree: Arc::new(tree),
        }
    }

    fn encoded(state: &State) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let sum = encode(state, &mut bytes)?;
        bytes[8..16].copy_from_slice(&sum.to_le_bytes());
        Ok(bytes)
    }

    fn decoded(bytes: &[u8]) -> Option<State> {
        decode(bytes, bytes.len())
    }

    fn reseal(bytes: &mut [u8]) {
        let parts: Vec<u64> = bytes[16..].chunks(PART).map(part).collect();
        let sum = checksum(bytes.len() - 16, &parts);
        bytes[8..16].copy_from_slice(&sum.to_le_bytes());
    }

    fn body_at(bytes: &[u8]) -> usize {
        let word = |at: usize| {
            u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
        };
        HEADER + word(72) + word(64) * 8
    }

    /// Every node as a line: its path and all it says.
    fn lines(state: &State) -> Vec<String> {
        fn describe(node: Node<'_>, path: &str, out: &mut Vec<String>) {
            out.push(format!(
                "{path} {:?} {} {} {} {:?} {} {:?} {}",
                node.kind(),
                node.bytes(),
                node.files(),
                node.dirs(),
                node.inode(),
                node.modified(),
                node.kinds(),
                node.read_error(),
            ));
            for child in node.children() {
                describe(child, &format!("{path}/{}", child.name()), out);
            }
        }
        let mut out = vec![format!(
            "{} {} {} {} {} {} {} {:?}",
            state.root,
            state.volume,
            state.root_id,
            state.journal,
            state.next,
            state.created,
            state.options,
            state.open
        )];
        describe(state.tree.root(), state.tree.root().name(), &mut out);
        out
    }

    #[test]
    fn a_saved_state_loads_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walk").join("walk-C.bin");
        let kept = state(sample());
        save(&path, &kept).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(lines(&loaded), lines(&kept));
    }

    #[test]
    fn a_state_past_one_part_streams_back_whole() {
        // Wide and long-named: more than a megabyte, names across parts.
        let files = (0..40_000)
            .map(|index| {
                let mut file = leaf(
                    &format!("{index:08}-{}", "n".repeat(index % 40)),
                    NodeKind::File,
                    index as u64,
                );
                file.inode = Some((VOLUME, index as u64 + 1));
                file
            })
            .collect();
        let kept = state(Tree::from_draft(
            dir("r", vec![dir("wide", files)]),
            Metric::Bytes,
        ));
        let bytes = encoded(&kept).unwrap();
        assert!(bytes.len() > PART);
        assert_eq!(lines(&decoded(&bytes).unwrap()), lines(&kept));
    }

    #[test]
    fn a_save_replaces_the_last_and_leaves_nothing_beside_it() {
        let temp = tempfile::tempdir().unwrap();
        // A directory the save makes: an elevated one refuses any other.
        let dir = temp.path().join("walk");
        let path = dir.join("walk.bin");
        let old = Tree::from_draft(Draft::directory("old"), Metric::Bytes);
        save(&path, &state(old)).unwrap();
        save(&path, &state(sample())).unwrap();
        assert_eq!(load(&path).unwrap().tree.root().name(), "C:\\");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["walk.bin"]);
    }

    #[test]
    fn a_missing_file_loads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("none.bin")).is_none());
    }

    #[test]
    fn any_damaged_byte_or_torn_end_is_refused() {
        let bytes = encoded(&state(sample())).unwrap();
        assert!(decoded(&bytes).is_some());
        for at in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x10;
            assert!(decoded(&damaged).is_none(), "byte {at}");
        }
        for len in 0..bytes.len() {
            assert!(decoded(&bytes[..len]).is_none(), "length {len}");
        }
    }

    #[test]
    fn a_checksummed_file_with_trailing_bytes_is_refused() {
        let mut bytes = encoded(&state(sample())).unwrap();
        bytes.push(0);
        reseal(&mut bytes);
        assert!(decoded(&bytes).is_none());
    }

    #[test]
    fn unknown_flags_and_classes_are_refused_even_when_checksummed() {
        let bytes = encoded(&state(sample())).unwrap();
        let root = body_at(&bytes);
        for (at, value) in [
            (root, 0x10 | DIRECTORY),  // an unknown tag bit
            (root + 1, 9),             // no such category
            (root + 1, (10 << 4) | 8), // no such reason
        ] {
            let mut forged = bytes.clone();
            forged[at] = value;
            reseal(&mut forged);
            assert!(decoded(&forged).is_none(), "{at}: {value:#x}");
        }
    }

    #[test]
    fn counts_that_do_not_add_up_are_refused() {
        let empty = Tree::from_draft(Draft::directory("r"), Metric::Bytes);
        let bytes = encoded(&state(empty)).unwrap();
        // The root's child count is just before its one-letter name and
        // that name's length.
        let mut forged = bytes.clone();
        let count_at = forged.len() - 3;
        forged[count_at] = 1;
        reseal(&mut forged);
        assert!(decoded(&forged).is_none());
        for nodes in [0, 2, MOST_NODES + 1] {
            let mut forged = bytes.clone();
            forged[NODE_COUNT_AT..NODE_COUNT_AT + 8]
                .copy_from_slice(&u64::to_le_bytes(nodes));
            reseal(&mut forged);
            assert!(decoded(&forged).is_none(), "{nodes} nodes");
        }
        let mut forged = bytes;
        forged[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        reseal(&mut forged);
        assert!(decoded(&forged).is_none());
    }

    #[test]
    fn names_that_are_not_one_component_are_neither_kept_nor_loaded() {
        let named = |name: &str| {
            state(Tree::from_draft(
                dir("r", vec![leaf(name, NodeKind::File, 1)]),
                Metric::Bytes,
            ))
        };
        let bytes = encoded(&named("ab")).unwrap();
        let at = bytes.windows(2).rposition(|pair| pair == b"ab").unwrap();
        for bad in ["..", "a/", "a\0", "C:", "a:"] {
            let mut forged = bytes.clone();
            forged[at..at + 2].copy_from_slice(bad.as_bytes());
            reseal(&mut forged);
            assert!(decoded(&forged).is_none(), "{bad:?}");
            assert!(encoded(&named(bad)).is_err(), "{bad:?}");
        }
        let mut forged = bytes;
        forged[at] = 0xFF;
        reseal(&mut forged);
        assert!(decoded(&forged).is_none(), "not UTF-8");
        for name in ["", "."] {
            assert!(encoded(&named(name)).is_err(), "{name:?}");
        }
    }

    #[test]
    fn depth_is_bounded_on_both_sides() {
        fn chain(depth: usize) -> Tree {
            let mut node = Draft::directory("d");
            for _ in 0..depth {
                node = dir("d", vec![node]);
            }
            Tree::from_draft(node, Metric::Bytes)
        }
        let deepest = encoded(&state(chain(MOST_DEPTH))).unwrap();
        let loaded = decoded(&deepest).unwrap();
        assert_eq!(loaded.tree.root().depth() as usize, MOST_DEPTH);
        assert!(encoded(&state(chain(MOST_DEPTH + 1))).is_err());
    }

    #[test]
    fn only_identities_of_the_volume_are_kept() {
        for (volume, kept) in [(VOLUME, true), (1, false)] {
            let mut file = leaf("f", NodeKind::File, 1);
            file.inode = Some((volume, 9));
            let tree = Tree::from_draft(dir("r", vec![file]), Metric::Bytes);
            assert_eq!(encoded(&state(tree)).is_ok(), kept, "{volume}");
        }
    }

    #[test]
    fn a_name_longer_than_an_entry_holds_is_not_loaded() {
        // Written by hand: no tree has such a name to save. A root "r" on
        // `C:/`, holding one file whose name is 64 KiB of `a`.
        let long = 1 << 16;
        let mut bytes = Vec::new();
        let mut out = Sealed::new(&mut bytes);
        out.write(&MAGIC).unwrap();
        // The checksum's place, then the header: volume, root id, journal,
        // next, created, options, open files, the root's length, nodes,
        // directories and name bytes.
        for value in [0, 1, 5, 1, 1, 1, 0, 0, 3, 2, 1, long] {
            out.u64(value).unwrap();
        }
        out.write(b"C:/").unwrap();
        // The root: modified, bytes, files, dirs, entries, then its name.
        out.write(&[DIRECTORY, 0]).unwrap();
        for value in [0, 1, 1, 1, 1, 1] {
            out.varint(value).unwrap();
        }
        out.write(b"r").unwrap();
        // The file: modified, size, then its name.
        out.write(&[crate::tree::FILE]).unwrap();
        for value in [0, 1, long] {
            out.varint(value).unwrap();
        }
        out.write(&vec![b'a'; long as usize]).unwrap();
        let sum = out.finish().unwrap();
        bytes[8..16].copy_from_slice(&sum.to_le_bytes());
        assert!(decoded(&bytes).is_none());
    }
}
