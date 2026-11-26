//! Linux directory enumeration & metadata backends behind the strategy-invariant
//! [`crate::backend::visit_dir_entries`] interface:
//!
//! 1. `visit_dir_statx`: `openat` + thread-local 64 KiB `getdents64` +
//!    synchronous relative `statx(dirfd, d_name, AT_SYMLINK_NOFOLLOW |
//!    AT_STATX_DONT_SYNC)` (with `statat` fallback on `ENOSYS`).
//! 2. `visit_dir_uring` (optional `io-uring` feature): same 64 KiB `getdents64`
//!    buffer, submitting up to 64 `IORING_OP_STATX` operations per single
//!    `io_uring_enter` syscall (automatically falling back to `visit_dir_statx`
//!    on tiny directories or if the kernel/seccomp disallows `io_uring`).

use std::cell::RefCell;
use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, RawDir, StatxFlags, makedev, openat,
    statat, statx,
};

use crate::backend::DirEntryMeta;
use crate::types::NodeKind;

/// 64 KiB `getdents64` read buffer reused per worker thread (zero per-dir
/// allocation; 32x larger than musl/glibc's default `readdir` buffer).
const GETDENTS_BUF_BYTES: usize = 64 * 1024;

const STATX_MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::INO)
    .union(StatxFlags::NLINK);

/// Flipped to `false` on the first `ENOSYS` so older kernels or strict seccomp
/// filters fall back to `statat(dirfd, ...)` cleanly without repeating errors.
static STATX_SUPPORTED: AtomicBool = AtomicBool::new(true);

thread_local! {
    static GETDENTS_BUF: RefCell<Box<[MaybeUninit<u8>]>> = RefCell::new(
        vec![MaybeUninit::uninit(); GETDENTS_BUF_BYTES].into_boxed_slice(),
    );
}

/// Strategy 1: `openat` + 64 KiB `getdents64` + synchronous relative `statx`.
pub fn visit_dir_statx<S, V>(
    dir_path: &Path,
    follow_links: bool,
    mut should_skip: S,
    mut visit: V,
) -> io::Result<()>
where
    S: FnMut(&[u8], bool) -> bool,
    V: FnMut(&[u8], io::Result<DirEntryMeta>) -> bool,
{
    let dirfd = openat(
        CWD,
        dir_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;

    GETDENTS_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let mut raw_dir = RawDir::new(&dirfd, &mut buf[..]);
        while let Some(entry_res) = raw_dir.next() {
            let entry = entry_res.map_err(io::Error::from)?;
            let c_name = entry.file_name();
            let raw_bytes = c_name.to_bytes();
            if raw_bytes == b"." || raw_bytes == b".." {
                continue;
            }
            let file_type = entry.file_type();
            if should_skip(raw_bytes, file_type == FileType::Directory) {
                continue;
            }
            let follow = follow_links && file_type == FileType::Symlink;
            let meta = stat_at(dirfd.as_fd(), c_name, follow);
            if !visit(raw_bytes, meta) {
                break;
            }
        }
        Ok(())
    })
}

/// Stat `name` relative to `dirfd` with `statx(..., AT_STATX_DONT_SYNC)` (or
/// `statat` fallback on `ENOSYS`), performing zero path allocations.
pub fn stat_at(
    dirfd: BorrowedFd<'_>,
    name: &CStr,
    follow_links: bool,
) -> io::Result<DirEntryMeta> {
    if STATX_SUPPORTED.load(Ordering::Relaxed) {
        let flags = if follow_links {
            AtFlags::STATX_DONT_SYNC
        } else {
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC
        };
        match statx(dirfd, name, flags, STATX_MASK) {
            Ok(st) => {
                return Ok(meta_from_statx(&st, follow_links));
            }
            Err(rustix::io::Errno::NOSYS | rustix::io::Errno::PERM) => {
                STATX_SUPPORTED.store(false, Ordering::Relaxed);
            }
            Err(err) => return Err(io::Error::from(err)),
        }
    }

    let flags = if follow_links {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    let st = statat(dirfd, name, flags).map_err(io::Error::from)?;
    let kind = kind_from_mode(st.st_mode);
    let device = st.st_dev;
    let allocated_bytes =
        u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512);
    let apparent_bytes = u64::try_from(st.st_size).unwrap_or(0);
    let modified = st.st_mtime;
    let inode = (st.st_ino != 0).then_some((device, st.st_ino));
    let hardlink = (kind == NodeKind::File
        && (follow_links || st.st_nlink > 1))
        .then_some(inode)
        .flatten();
    Ok(DirEntryMeta {
        kind,
        device,
        allocated_bytes,
        apparent_bytes,
        modified,
        hardlink,
        inode,
        hidden_attr: false,
        evicted: false,
    })
}

fn meta_from_statx(st: &rustix::fs::Statx, follow_links: bool) -> DirEntryMeta {
    let kind = kind_from_mode(u32::from(st.stx_mode));
    let device = makedev(st.stx_dev_major, st.stx_dev_minor);
    let allocated_bytes = st.stx_blocks.saturating_mul(512);
    let apparent_bytes = st.stx_size;
    let modified = st.stx_mtime.tv_sec;
    let inode = (st.stx_ino != 0).then_some((device, st.stx_ino));
    let hardlink = (kind == NodeKind::File
        && (follow_links || st.stx_nlink > 1))
        .then_some(inode)
        .flatten();
    DirEntryMeta {
        kind,
        device,
        allocated_bytes,
        apparent_bytes,
        modified,
        hardlink,
        inode,
        hidden_attr: false,
        evicted: false,
    }
}

const fn kind_from_mode(mode: u32) -> NodeKind {
    // POSIX S_IFMT = 0o170000
    match mode & 0o170_000 {
        0o040_000 => NodeKind::Directory,
        0o100_000 => NodeKind::File,
        0o120_000 => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

#[cfg(feature = "io-uring")]
#[allow(
    unsafe_code,
    reason = "io_uring SQE submission passes pinned ring-owned buffers to the kernel"
)]
mod uring {
    use std::cell::RefCell;
    use std::ffi::CStr;
    use std::io;
    use std::mem::MaybeUninit;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use io_uring::{IoUring, opcode, types};
    use rustix::fd::{AsFd, AsRawFd};
    use rustix::fs::{
        AtFlags, CWD, FileType, Mode, OFlags, RawDir, Statx, openat,
    };

    use super::{
        GETDENTS_BUF, STATX_MASK, meta_from_statx, stat_at, visit_dir_statx,
    };
    use crate::backend::DirEntryMeta;

    /// Number of `IORING_OP_STATX` operations submitted per `io_uring_enter`.
    const URING_BATCH: usize = 64;
    /// Below this threshold, synchronous `statx` avoids ring setup/enter cost.
    const MIN_URING_BATCH: usize = 8;

    static URING_AVAILABLE: AtomicBool = AtomicBool::new(true);

    struct Pending {
        /// Byte range of NUL-terminated `d_name` inside `name_slab` (including
        /// the trailing NUL byte so `as_ptr()` is a valid C string).
        start: usize,
        end: usize,
        follow: bool,
    }

    struct UringThreadState {
        ring: IoUring,
        /// Heap-pinned array of kernel `Statx` output buffers (1 per slot).
        statx_bufs: Box<[Statx; URING_BATCH]>,
        /// Contiguous NUL-terminated name copy for the active batch (reused
        /// across batches with zero per-batch allocation).
        name_slab: Vec<u8>,
        pending: Vec<Pending>,
        results: Vec<i32>,
    }

    impl UringThreadState {
        fn try_new() -> io::Result<Self> {
            let ring = IoUring::new((URING_BATCH * 2) as u32)?;
            // SAFETY: `Statx` is a plain C POD struct of integers.
            let zeroed: Statx = unsafe { MaybeUninit::zeroed().assume_init() };
            Ok(Self {
                ring,
                statx_bufs: Box::new([zeroed; URING_BATCH]),
                name_slab: Vec::with_capacity(4096),
                pending: Vec::with_capacity(URING_BATCH),
                results: vec![0; URING_BATCH],
            })
        }

        fn flush_batch<V>(
            &mut self,
            dirfd: rustix::fd::BorrowedFd<'_>,
            visit: &mut V,
        ) -> io::Result<bool>
        where
            V: FnMut(&[u8], io::Result<DirEntryMeta>) -> bool,
        {
            let count = self.pending.len();
            if count == 0 {
                return Ok(true);
            }

            // Tiny batch fast path: avoid `io_uring_enter` overhead when a
            // directory only has a handful of entries.
            if count < MIN_URING_BATCH {
                for slot in &self.pending {
                    let bytes_with_nul = &self.name_slab[slot.start..slot.end];
                    let c_name = CStr::from_bytes_with_nul(bytes_with_nul)
                        .expect("nul-terminated d_name");
                    let raw = &bytes_with_nul[..bytes_with_nul.len() - 1];
                    let meta = stat_at(dirfd, c_name, slot.follow);
                    if !visit(raw, meta) {
                        self.pending.clear();
                        self.name_slab.clear();
                        return Ok(false);
                    }
                }
                self.pending.clear();
                self.name_slab.clear();
                return Ok(true);
            }

            let raw_fd = types::Fd(dirfd.as_raw_fd());
            let mask = STATX_MASK.bits();
            let base_flags_nofollow = (AtFlags::SYMLINK_NOFOLLOW
                | AtFlags::STATX_DONT_SYNC)
                .bits()
                .cast_signed();
            let base_flags_follow =
                AtFlags::STATX_DONT_SYNC.bits().cast_signed();

            {
                let mut sq = self.ring.submission();
                for (i, slot) in self.pending.iter().enumerate() {
                    let path_ptr = self.name_slab[slot.start..slot.end]
                        .as_ptr()
                        .cast::<std::ffi::c_char>();
                    let statx_ptr =
                        (&raw mut self.statx_bufs[i]).cast::<types::statx>();
                    let flags = if slot.follow {
                        base_flags_follow
                    } else {
                        base_flags_nofollow
                    };
                    let sqe = opcode::Statx::new(raw_fd, path_ptr, statx_ptr)
                        .flags(flags)
                        .mask(mask)
                        .build()
                        .user_data(i as u64);
                    // SAFETY: `self.name_slab` is not mutated/reallocated and
                    // `self.statx_bufs` is heap-pinned until after
                    // `submit_and_wait(count)` finishes draining all `count`
                    // CQEs below.
                    unsafe {
                        sq.push(&sqe).map_err(|_| {
                            io::Error::other("io_uring submission queue full")
                        })?;
                    }
                }
            }

            self.ring.submit_and_wait(count)?;

            let mut completed = 0usize;
            while completed < count {
                {
                    let mut cq = self.ring.completion();
                    for cqe in &mut cq {
                        let idx = cqe.user_data() as usize;
                        if idx < count {
                            self.results[idx] = cqe.result();
                        }
                        completed += 1;
                    }
                }
                if completed < count {
                    self.ring.submit_and_wait(count - completed)?;
                }
            }

            let mut keep_going = true;
            for (i, slot) in self.pending.iter().enumerate() {
                let raw = &self.name_slab[slot.start..slot.end - 1];
                let res = self.results[i];
                let meta = if res == 0 {
                    Ok(meta_from_statx(&self.statx_bufs[i], slot.follow))
                } else {
                    Err(io::Error::from_raw_os_error(-res))
                };
                if !visit(raw, meta) {
                    keep_going = false;
                    break;
                }
            }

            self.pending.clear();
            self.name_slab.clear();
            Ok(keep_going)
        }
    }

    thread_local! {
        static URING_STATE: RefCell<Option<UringThreadState>> =
            const { RefCell::new(None) };
    }

    /// Strategy 2: `openat` + 64 KiB `getdents64` + batched `io_uring`
    /// `IORING_OP_STATX` (transparent fallback to `visit_dir_statx` if blocked).
    pub fn visit_dir_uring<S, V>(
        dir_path: &Path,
        follow_links: bool,
        mut should_skip: S,
        mut visit: V,
    ) -> io::Result<()>
    where
        S: FnMut(&[u8], bool) -> bool,
        V: FnMut(&[u8], io::Result<DirEntryMeta>) -> bool,
    {
        if !URING_AVAILABLE.load(Ordering::Relaxed) {
            return visit_dir_statx(dir_path, follow_links, should_skip, visit);
        }

        let initialized = URING_STATE.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                if let Ok(state) = UringThreadState::try_new() {
                    *opt = Some(state);
                } else {
                    URING_AVAILABLE.store(false, Ordering::Relaxed);
                    return false;
                }
            }
            true
        });

        if !initialized {
            return visit_dir_statx(dir_path, follow_links, should_skip, visit);
        }

        let dirfd = openat(
            CWD,
            dir_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;

        GETDENTS_BUF.with(|buf_cell| {
            URING_STATE.with(|ring_cell| {
                let mut buf = buf_cell.borrow_mut();
                let mut ring_opt = ring_cell.borrow_mut();
                let state = ring_opt
                    .as_mut()
                    .expect("io_uring thread state initialized above");
                state.pending.clear();
                state.name_slab.clear();

                let mut raw_dir = RawDir::new(&dirfd, &mut buf[..]);
                let mut keep_going = true;
                while let Some(entry_res) = raw_dir.next() {
                    let entry = entry_res.map_err(io::Error::from)?;
                    let c_name = entry.file_name();
                    let raw_bytes = c_name.to_bytes();
                    if raw_bytes == b"." || raw_bytes == b".." {
                        continue;
                    }
                    let file_type = entry.file_type();
                    if should_skip(raw_bytes, file_type == FileType::Directory)
                    {
                        continue;
                    }
                    let start = state.name_slab.len();
                    state
                        .name_slab
                        .extend_from_slice(c_name.to_bytes_with_nul());
                    let end = state.name_slab.len();
                    state.pending.push(Pending {
                        start,
                        end,
                        follow: follow_links && file_type == FileType::Symlink,
                    });
                    if state.pending.len() >= URING_BATCH {
                        keep_going =
                            state.flush_batch(dirfd.as_fd(), &mut visit)?;
                        if !keep_going {
                            break;
                        }
                    }
                }
                if keep_going {
                    let _ = state.flush_batch(dirfd.as_fd(), &mut visit)?;
                }
                Ok(())
            })
        })
    }
}

#[cfg(feature = "io-uring")]
pub use uring::visit_dir_uring;
