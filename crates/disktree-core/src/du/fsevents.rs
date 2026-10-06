//! macOS: what changed under a directory since an earlier moment, from the
//! `FSEvents` journal that `fseventsd` keeps on every local volume.
//!
//! A snapshot records the journal's event id from before its walk and the
//! volume's journal UUID. Replaying the journal from that id names every
//! directory whose entries changed since, and every directory holding a
//! file that was written and closed. A UUID that no longer matches means
//! the journal was reset, and nothing it says can be relied on.
//!
//! The journal says nothing about a file still open for writing until it
//! is closed; `super::refresh` stats the files most likely to be in that
//! state again whatever the journal says.

#![allow(
    unsafe_code,
    reason = "CoreServices' FSEvents calls, which std does not wrap; each \
              block says why it is sound"
)]

use std::ffi::{CStr, c_char, c_void};
use std::time::{Duration, Instant};

type CFTypeRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFRunLoopRef = *const c_void;
type FSEventStreamRef = *mut c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    bytes: [u8; 16],
}

type Callback = extern "C" fn(
    FSEventStreamRef,
    *mut c_void,
    usize,
    *mut c_void,
    *const u32,
    *const u64,
);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFUUIDRef;
    fn FSEventStreamCreate(
        allocator: CFTypeRef,
        callback: Callback,
        context: *const FSEventStreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamFlushSync(stream: FSEventStreamRef);
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: [usize; 5];
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRelease(value: CFTypeRef);
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: u8,
    ) -> CFStringRef;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
}

const UTF8: u32 = 0x0800_0100;
const MUST_SCAN_SUBDIRS: u32 = 0x1;
const USER_DROPPED: u32 = 0x2;
const KERNEL_DROPPED: u32 = 0x4;
const IDS_WRAPPED: u32 = 0x8;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;

/// How long to wait for the journal to finish replaying before giving up
/// on it; a replay that long would cost more than it saves.
const REPLAY_LIMIT: Duration = Duration::from_secs(10);

/// The journal position to record before a walk: its current event id.
pub fn current_event() -> u64 {
    // SAFETY: no arguments, returns a plain integer.
    unsafe { FSEventsGetCurrentEventId() }
}

/// The journal UUID of the volume `dev` is, or `None` when that volume
/// keeps no journal.
pub fn volume_uuid(dev: u64) -> Option<[u8; 16]> {
    let dev = libc::dev_t::try_from(dev.cast_signed()).ok()?;
    // SAFETY: takes a device number by value and returns a new CFUUID we
    // own, or NULL.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(dev) };
    if uuid.is_null() {
        return None;
    }
    // SAFETY: `uuid` is a valid CFUUID until the CFRelease below.
    let bytes = unsafe { CFUUIDGetUUIDBytes(uuid) };
    // SAFETY: we own `uuid` from the Copy call above and release it once.
    unsafe { CFRelease(uuid) };
    Some(bytes.bytes)
}

pub use super::refresh::Changes;

struct Collector {
    changes: Changes,
    done: bool,
    lost: bool,
}

extern "C" fn collect(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: `info` is the `Collector` that `changes_since` passed in the
    // stream's context and keeps alive, and borrowed by nothing else, for
    // as long as the stream can call back; callbacks run on that same
    // thread, inside its run-loop or flush calls. Without
    // `kFSEventStreamCreateFlagUseCFTypes`, `paths` is an array of `count`
    // NUL-terminated C strings and `flags` has `count` entries, valid for
    // the duration of this call.
    let collector = unsafe { &mut *info.cast::<Collector>() };
    let paths = paths.cast::<*const c_char>();
    for at in 0..count {
        // SAFETY: see above; `at < count`.
        let (path, flag) = unsafe {
            (CStr::from_ptr(*paths.add(at)).to_bytes(), *flags.add(at))
        };
        if flag & HISTORY_DONE != 0 {
            collector.done = true;
            continue;
        }
        if flag & (USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED | ROOT_CHANGED)
            != 0
        {
            collector.lost = true;
            continue;
        }
        let path = path.strip_suffix(b"/").unwrap_or(path).to_vec();
        if flag & (MUST_SCAN_SUBDIRS | MOUNT | UNMOUNT) != 0 {
            collector.changes.subtrees.push(path);
        } else {
            collector.changes.dirs.push(path);
        }
    }
}

/// Every change the journal recorded under `root` (an absolute, canonical
/// path) since `since`. `None` when the journal cannot say: it dropped
/// events, was reset, or did not finish replaying in time.
pub fn changes_since(root: &[u8], since: u64) -> Option<Changes> {
    let mut collector = Collector {
        changes: Changes::default(),
        done: false,
        lost: false,
    };
    let context = FSEventStreamContext {
        version: 0,
        info: (&raw mut collector).cast(),
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    let len = isize::try_from(root.len()).ok()?;
    // SAFETY: `root` is a live byte slice of `len` bytes; CoreFoundation
    // copies it into a new CFString we own, or returns NULL.
    let path = unsafe {
        CFStringCreateWithBytes(std::ptr::null(), root.as_ptr(), len, UTF8, 0)
    };
    if path.is_null() {
        return None;
    }
    let values = [path];
    // SAFETY: `values` holds one valid CFString; the type callbacks make
    // the array retain it, so our own reference is released right after.
    let paths = unsafe {
        CFArrayCreate(
            std::ptr::null(),
            values.as_ptr(),
            1,
            (&raw const kCFTypeArrayCallBacks).cast(),
        )
    };
    // SAFETY: we own `path` from its Create call and release it once.
    unsafe { CFRelease(path) };
    if paths.is_null() {
        return None;
    }
    // SAFETY: `context` and `collector` outlive the stream, which is
    // stopped, invalidated and released before this function returns;
    // `paths` is a valid CFArray of CFStrings.
    let stream = unsafe {
        FSEventStreamCreate(
            std::ptr::null(),
            collect,
            &raw const context,
            paths,
            since,
            0.0,
            0,
        )
    };
    // SAFETY: the stream retains `paths` if it needs it; we own ours.
    unsafe { CFRelease(paths) };
    if stream.is_null() {
        return None;
    }
    // SAFETY: `stream` is a valid, unscheduled stream; the run loop is this
    // thread's, and the mode constant is CoreFoundation's own.
    let started = unsafe {
        FSEventStreamScheduleWithRunLoop(
            stream,
            CFRunLoopGetCurrent(),
            kCFRunLoopDefaultMode,
        );
        FSEventStreamStart(stream) != 0
    };
    let deadline = Instant::now() + REPLAY_LIMIT;
    if started {
        while !collector.done && !collector.lost && Instant::now() < deadline {
            // SAFETY: runs this thread's run loop briefly, delivering the
            // stream's callbacks here, where `collector` is not borrowed.
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 1);
            }
        }
        // SAFETY: a started stream; delivers anything still pending
        // through the callback, synchronously, on this thread.
        unsafe { FSEventStreamFlushSync(stream) };
    }
    // SAFETY: tearing down the stream created above, in the documented
    // order, exactly once.
    unsafe {
        if started {
            FSEventStreamStop(stream);
        }
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }
    (started && collector.done && !collector.lost).then_some(collector.changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_journal_names_a_directory_written_in() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical");
        let dev = std::os::unix::fs::MetadataExt::dev(
            &std::fs::metadata(&root).expect("stat"),
        );
        if volume_uuid(dev).is_none() {
            eprintln!("SKIPPED: this volume keeps no FSEvents journal");
            return;
        }
        let since = current_event();
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub/file"), b"x").expect("write");
        let root_bytes = root.as_os_str().as_encoded_bytes();
        let sub = root.join("sub");
        let sub = sub.as_os_str().as_encoded_bytes();
        // fseventsd writes its history asynchronously; give it a moment.
        let mut seen = false;
        for _ in 0..50 {
            let changes = changes_since(root_bytes, since).expect("replays");
            if changes.dirs.iter().any(|dir| dir.as_slice() == sub) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(seen, "the write in sub/ was reported");
    }
}
