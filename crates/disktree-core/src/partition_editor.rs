//! Hand a local disk to `GParted`, which owns validation and applying changes.
//!
//! Discovery is read-only. No partition, filesystem or encryption command is
//! run by disktree; the editor asks for its own authorization and confirmation.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

/// An editor and the single physical disk backing a local mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub disk: PathBuf,
    pub partition: Option<PathBuf>,
    pub editor: PathBuf,
}

/// Resolve the current mount rather than trusting a cached device label.
pub fn prepare(root: &Path) -> io::Result<Target> {
    let editor = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join("gparted"))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Install GParted to resize partitions, then try again.",
            )
        })?;
    let mount = output(
        Command::new("findmnt")
            // Btrfs mount IDs can have a virtual major:minor pair; use the
            // real source and omit the subvolume suffix (`[/@home]`). JSON
            // preserves device names containing whitespace without splitting.
            .args([
                "--json", "--evaluate", "--nofsroot", "--output", "SOURCE",
                "--target",
            ])
            .arg(root),
    )?;
    let device = device_path(&mount)?;
    let topology = output(
        Command::new("lsblk")
            .args([
                "--inverse",
                // PATH alone does not imply tree-shaped JSON.
                "--tree",
                "--json",
                "--paths",
                "--output",
                "PATH,TYPE",
                "--",
            ])
            .arg(device),
    )?;
    let (disk, partition) = physical_disk(&topology)?;
    Ok(Target {
        disk,
        partition,
        editor,
    })
}

/// Wait off the UI thread, so the caller can refresh after the editor exits.
pub fn open(target: &Target) -> io::Result<()> {
    // Launch the installed wrapper as the user. GParted owns elevation; the
    // treemap must never run as root just to open a partition editor.
    output(Command::new(&target.editor).arg(&target.disk)).map(|_| ())
}

fn output(command: &mut Command) -> io::Result<String> {
    let result = command.stdin(Stdio::null()).output()?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr);
        return Err(io::Error::other(format!(
            "{} exited with {}. {}",
            command.get_program().to_string_lossy(),
            result.status,
            detail.trim().chars().take(500).collect::<String>()
        )));
    }
    String::from_utf8(result.stdout).map_err(io::Error::other)
}

fn device_path(mount: &str) -> io::Result<PathBuf> {
    let mounts: Mounts =
        serde_json::from_str(mount).map_err(io::Error::other)?;
    if let [filesystem] = mounts.filesystems.as_slice()
        && filesystem.source.starts_with("/dev")
    {
        return Ok(filesystem.source.clone());
    }
    Err(io::Error::other(
        "Cannot identify this mount's block device.",
    ))
}

#[derive(Debug, Deserialize)]
struct Mounts {
    filesystems: Vec<Filesystem>,
}

#[derive(Debug, Deserialize)]
struct Filesystem {
    source: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Topology {
    blockdevices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
struct Device {
    path: PathBuf,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    children: Vec<Self>,
}

fn physical_disk(json: &str) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let topology: Topology =
        serde_json::from_str(json).map_err(io::Error::other)?;
    let [device] = topology.blockdevices.as_slice() else {
        return Err(unsupported());
    };
    let mut device = device;
    let mut partition = None;
    loop {
        // Only an unambiguous chain through partitions and LUKS is supported.
        // Picking one member of LVM, RAID or a multipath device is misleading.
        match (device.kind.as_str(), device.children.as_slice()) {
            ("disk", []) if device.path.starts_with("/dev") => {
                return Ok((device.path.clone(), partition));
            }
            ("part", [parent]) if partition.is_none() => {
                partition = Some(device.path.clone());
                device = parent;
            }
            ("crypt", [parent]) => device = parent,
            _ => return Err(unsupported()),
        }
    }
}

fn unsupported() -> io::Error {
    io::Error::other(
        "This volume does not resolve to one supported physical disk. \
         Open your storage manager separately for LVM, RAID or virtual disks.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_partition_and_a_luks_mapping_to_the_whole_disk() {
        for (leaf, expected_disk, expected_partition) in [
            (
                r#"{"path":"/dev/nvme0n1p5","type":"part","children":[
                {"path":"/dev/nvme0n1","type":"disk"}]}"#,
                "/dev/nvme0n1",
                Some("/dev/nvme0n1p5"),
            ),
            (
                r#"{"path":"/dev/mapper/root","type":"crypt","children":[
                {"path":"/dev/nvme0n1p5","type":"part","children":[
                    {"path":"/dev/nvme0n1","type":"disk"}]}]}"#,
                "/dev/nvme0n1",
                Some("/dev/nvme0n1p5"),
            ),
            (r#"{"path":"/dev/sda","type":"disk"}"#, "/dev/sda", None),
        ] {
            let json = format!(r#"{{"blockdevices":[{leaf}]}}"#);
            let (disk, partition) =
                physical_disk(&json).expect("one physical disk");
            assert_eq!(disk, Path::new(expected_disk));
            assert_eq!(partition.as_deref(), expected_partition.map(Path::new));
        }
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_storage() {
        for json in [
            r#"{"blockdevices":[]}"#,
            r#"{"blockdevices":[{"path":"/dev/a","type":"disk"},
                {"path":"/dev/b","type":"disk"}]}"#,
            r#"{"blockdevices":[{"path":"/dev/vg/root","type":"lvm",
                "children":[{"path":"/dev/sda","type":"disk"}]}]}"#,
            r#"{"blockdevices":[{"path":"/dev/md0","type":"raid1",
                "children":[{"path":"/dev/sda","type":"disk"},
                    {"path":"/dev/sdb","type":"disk"}]}]}"#,
            r#"{"blockdevices":[{"path":"/dev/loop0","type":"loop"}]}"#,
            r#"{"blockdevices":[{"path":"/dev/sda1","type":"part"}]}"#,
            r#"{"blockdevices":[{"path":"/tmp/disk","type":"disk"}]}"#,
            "not json",
        ] {
            assert!(physical_disk(json).is_err(), "{json}");
        }
    }

    #[test]
    fn mount_source_requires_one_block_device_and_preserves_whitespace() {
        for source in ["/dev/mapper/omarchy_root", "/dev/mapper/my disk"] {
            let mount =
                format!(r#"{{"filesystems":[{{"source":"{source}"}}]}}"#);
            assert_eq!(device_path(&mount).unwrap(), Path::new(source));
        }
        for invalid in [
            "not json",
            r#"{"filesystems":[]}"#,
            r#"{"filesystems":[{"source":"tmpfs"}]}"#,
            r#"{"filesystems":[{"source":"server:/share"}]}"#,
            r#"{"filesystems":[{"source":"/dev/sda1"},{"source":"/dev/sdb1"}]}"#,
        ] {
            assert!(device_path(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn passes_the_disk_as_one_argument_and_reports_editor_failure() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::TempDir::new().unwrap();
        let editor = temp.path().join("editor");
        std::fs::write(
            &editor,
            "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = '/dev/disk with spaces' ]\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &editor,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let target = Target {
            disk: PathBuf::from("/dev/disk with spaces"),
            partition: None,
            editor,
        };
        open(&target).expect("disk is passed literally");
        std::fs::write(&target.editor, "#!/bin/sh\necho denied >&2\nexit 1\n")
            .unwrap();
        assert!(open(&target).unwrap_err().to_string().contains("denied"));
    }
}
