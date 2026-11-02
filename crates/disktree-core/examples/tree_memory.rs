//! What a scanned tree costs to hold and to let go of, on macOS:
//! `cargo run --release -p disktree-core --example tree_memory -- PATH`
//!
//! Scans PATH, then reports the process's physical footprint (resident
//! plus compressed, what Activity Monitor calls Memory) after the scan and
//! at its highest so far; the time to copy the tree, to re-rank it as the
//! size/files toggle does, to derive its totals again and to classify it;
//! and the time and page faults of dropping each copy. Run it in a fresh
//! process each time: a drop's cost depends on what the allocator and the
//! compressor hold.

#![allow(
    unsafe_code,
    reason = "getrusage and proc_pid_rusage, which std does not wrap"
)]
#![allow(clippy::cast_precision_loss, reason = "report figures")]

#[cfg(target_os = "macos")]
fn main() -> std::io::Result<()> {
    use std::path::PathBuf;
    use std::time::Instant;

    use disktree_core::scan::{ScanOptions, scan};
    use disktree_core::tree::{Metric, aggregate, reorder};

    let root = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);

    let (before, _) = footprint_mib();
    let started = Instant::now();
    let tree = scan(&root, ScanOptions::default())?;
    let scanned = started.elapsed().as_secs_f64();
    let (now, peak) = footprint_mib();
    let storage = tree.storage_bytes();
    let (allocations, sharings, both) = tree.side_records();
    println!(
        "scan={scanned:.2}s files={} dirs={} footprint+={:.0}MiB \
         peak+={:.0}MiB storage_used={:.0}MiB storage_reserved={:.0}MiB \
         allocations={allocations} sharings={sharings} both={both}",
        tree.root().files,
        tree.root().dirs,
        now - before,
        peak - before,
        storage.used as f64 / MIB,
        storage.reserved as f64 / MIB,
    );

    let started = Instant::now();
    let mut copy = tree.clone();
    let cloned = started.elapsed().as_secs_f64();
    let started = Instant::now();
    reorder(&mut copy, Metric::Files);
    let reordered = started.elapsed().as_secs_f64();
    let started = Instant::now();
    aggregate(&mut copy, Metric::Files);
    let aggregated = started.elapsed().as_secs_f64();
    let started = Instant::now();
    disktree_core::classify::classify(&mut copy);
    let classified = started.elapsed().as_secs_f64();
    let (now, peak) = footprint_mib();
    println!(
        "copy={cloned:.2}s reorder={reordered:.2}s aggregate={aggregated:.2}s \
         classify={classified:.3}s footprint+={:.0}MiB peak+={:.0}MiB",
        now - before,
        peak - before,
    );
    timed_drop("copy", copy);
    timed_drop("scanned tree", tree);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("tree_memory reads macOS process counters; it runs on macOS");
}

#[cfg(target_os = "macos")]
const MIB: f64 = 1024.0 * 1024.0;

#[cfg(target_os = "macos")]
fn faults() -> (i64, i64) {
    // SAFETY: getrusage fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    (usage.ru_minflt, usage.ru_majflt)
}

/// Resident plus compressed, now and at its highest: plain RSS leaves out
/// what the compressor holds, which on a large tree is most of it.
#[cfg(target_os = "macos")]
fn footprint_mib() -> (f64, f64) {
    // SAFETY: proc_pid_rusage fills the v4 struct it is given.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let pid = i32::try_from(std::process::id()).expect("a pid fits");
    // SAFETY: as above.
    let status = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast())
    };
    // A zeroed struct would read as a footprint of nothing.
    assert_eq!(
        status,
        0,
        "proc_pid_rusage: {}",
        std::io::Error::last_os_error()
    );
    (
        info.ri_phys_footprint as f64 / MIB,
        info.ri_lifetime_max_phys_footprint as f64 / MIB,
    )
}

#[cfg(target_os = "macos")]
fn timed_drop<T>(label: &str, value: T) {
    let (minor, major) = faults();
    let started = std::time::Instant::now();
    drop(value);
    let seconds = started.elapsed().as_secs_f64();
    let (minor_after, major_after) = faults();
    println!(
        "{label}: drop={seconds:.3}s minor_faults={} major_faults={}",
        minor_after - minor,
        major_after - major
    );
}
