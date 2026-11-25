//! Checked scanner payloads appended after the desktop executable.
//!
//! Footer: manifest JSON, its little-endian u64 length, then a fixed magic.
//! Offsets are absolute; readers bound every range before allocating. This
//! detects damaged packages, not maliciously replaced desktop executables.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const MAGIC: &[u8; 16] = b"DISKTREE-SSH-V1!";
const FOOTER: u64 = 24;
const MAX_MANIFEST: u64 = 64 << 10;
const MAX_PAYLOAD: u64 = 64 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub arch: String,
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    base_length: u64,
    entries: Vec<Entry>,
}

#[derive(Clone, Debug)]
pub struct Archive {
    path: PathBuf,
    manifest: Manifest,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Archive {
    pub fn current() -> io::Result<Option<Self>> {
        // On Linux, `current_exe()` returns `... (deleted)` after an in-place
        // upgrade (`make install` or package update) while the app is open,
        // causing `File::open` to fail with ENOENT. Opening `/proc/self/exe`
        // directly resolves the live running inode instead.
        #[cfg(target_os = "linux")]
        {
            Self::open(Path::new("/proc/self/exe"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::open(&std::env::current_exe()?)
        }
    }

    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if length < FOOTER {
            return Ok(None);
        }
        file.seek(SeekFrom::End(-16))?;
        let mut magic = [0; 16];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(length - FOOTER))?;
        let mut size = [0; 8];
        file.read_exact(&mut size)?;
        let size = u64::from_le_bytes(size);
        if size > MAX_MANIFEST || size > length - FOOTER {
            return Err(io::Error::other("invalid scanner manifest length"));
        }
        let start = length - FOOTER - size;
        file.seek(SeekFrom::Start(start))?;
        let mut json = vec![0; size as usize];
        file.read_exact(&mut json)?;
        let manifest: Manifest =
            serde_json::from_slice(&json).map_err(io::Error::other)?;
        if manifest.version != 1
            || manifest.entries.len() > 8
            || manifest.base_length > start
        {
            return Err(io::Error::other("unsupported scanner manifest"));
        }
        let mut end = manifest.base_length;
        let mut arches = std::collections::HashSet::new();
        for entry in &manifest.entries {
            if !matches!(entry.arch.as_str(), "x86_64" | "aarch64")
                || !arches.insert(&entry.arch)
                || entry.offset != end
                || entry.length == 0
                || entry.length > MAX_PAYLOAD
                || entry.length > start.saturating_sub(entry.offset)
                || entry.sha256.len() != 64
                || !entry.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            {
                return Err(io::Error::other("invalid scanner payload range"));
            }
            end = entry
                .offset
                .checked_add(entry.length)
                .ok_or_else(|| io::Error::other("scanner range overflow"))?;
        }
        if end != start {
            return Err(io::Error::other(
                "scanner payload has unindexed bytes",
            ));
        }
        Ok(Some(Self {
            path: path.into(),
            manifest,
        }))
    }

    pub fn entries(&self) -> &[Entry] {
        &self.manifest.entries
    }

    pub fn load(&self, arch: &str) -> io::Result<(Entry, Vec<u8>)> {
        let entry = self
            .entries()
            .iter()
            .find(|entry| entry.arch == arch)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "this disktree build has no embedded Linux {arch} scanner"
                ))
            })?
            .clone();
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut bytes = vec![0; entry.length as usize];
        file.read_exact(&mut bytes)?;
        if digest(&bytes) != entry.sha256 {
            return Err(io::Error::other("embedded scanner checksum mismatch"));
        }
        Ok((entry, bytes))
    }
}

/// Append after stripping, before distributing. Repeating this replaces the
/// old trailer instead of growing the executable on every incremental build.
pub fn append(binary: &Path, helpers: &[(String, PathBuf)]) -> io::Result<()> {
    let previous = Archive::open(binary)?;
    let mut file = OpenOptions::new().read(true).write(true).open(binary)?;
    let base_length = previous.map_or_else(
        || file.metadata().map(|meta| meta.len()),
        |archive| Ok(archive.manifest.base_length),
    )?;
    let mut entries = Vec::new();
    // Read and validate everything before changing the executable.
    let mut payloads = Vec::new();
    let mut offset = base_length;
    for (arch, path) in helpers {
        let bytes = std::fs::read(path)?;
        validate_static_elf(&bytes, arch)?;
        entries.push(Entry {
            arch: arch.clone(),
            offset,
            length: bytes.len() as u64,
            sha256: digest(&bytes),
        });
        offset += bytes.len() as u64;
        payloads.push(bytes);
    }
    let json = serde_json::to_vec(&Manifest {
        version: 1,
        base_length,
        entries,
    })
    .map_err(io::Error::other)?;
    file.set_len(base_length)?;
    file.seek(SeekFrom::Start(base_length))?;
    for bytes in payloads {
        file.write_all(&bytes)?;
    }
    file.write_all(&json)?;
    file.write_all(&(json.len() as u64).to_le_bytes())?;
    file.write_all(MAGIC)?;
    file.sync_all()
}

/// Our portability promise is static ELF64, not merely a musl filename.
pub fn validate_static_elf(bytes: &[u8], arch: &str) -> io::Result<()> {
    let invalid = || io::Error::other("scanner must be static Linux ELF64");
    if bytes.len() < 64 || &bytes[..6] != b"\x7fELF\x02\x01" {
        return Err(invalid());
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if !matches!((arch, machine), ("x86_64", 62) | ("aarch64", 183)) {
        return Err(io::Error::other("scanner ELF architecture mismatch"));
    }
    let start = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as u64;
    if size < 56
        || count == 0
        || start
            .checked_add(size * count)
            .is_none_or(|end| end > bytes.len() as u64)
    {
        return Err(invalid());
    }
    for index in 0..count {
        let offset = (start + index * size) as usize;
        let kind =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        // PT_INTERP would require a particular libc loader on the server.
        if kind == 3 {
            return Err(io::Error::other("scanner has a dynamic ELF loader"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 120];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn trailer_roundtrips_replaces_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let helper = dir.path().join("helper");
        std::fs::write(&app, b"desktop").unwrap();
        assert!(Archive::open(&app).unwrap().is_none());
        std::fs::write(&helper, elf()).unwrap();
        let inputs = [("x86_64".into(), helper)];
        append(&app, &inputs).unwrap();
        let size = app.metadata().unwrap().len();
        append(&app, &inputs).unwrap();
        assert_eq!(app.metadata().unwrap().len(), size);
        let archive = Archive::open(&app).unwrap().unwrap();
        assert_eq!(archive.load("x86_64").unwrap().1, elf());
        assert!(archive.load("aarch64").is_err());
        let mut bytes = std::fs::read(&app).unwrap();
        bytes[8] ^= 1;
        std::fs::write(&app, bytes).unwrap();
        assert!(archive.load("x86_64").is_err());
    }

    #[test]
    fn dynamic_or_wrong_arch_helpers_are_refused() {
        let mut bytes = elf();
        assert!(validate_static_elf(&bytes, "aarch64").is_err());
        bytes[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(validate_static_elf(&bytes, "x86_64").is_err());
    }

    #[test]
    fn oversized_footer_is_refused_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad");
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(MAGIC);
        std::fs::write(&path, bytes).unwrap();
        assert!(Archive::open(&path).is_err());
    }
}
