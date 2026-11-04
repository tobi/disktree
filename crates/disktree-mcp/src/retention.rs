//! A read-only retention preflight, deliberately without byte attribution.

use std::path::Path;

const LIMITS: &str = "Retained bytes for the selected paths: unknown. Immediate recovered bytes: unknown. Time Machine snapshot presence is a plausible retention mechanism, not proof of attribution. This does not enumerate every APFS snapshot, open file or later volume write. Even no listed Time Machine snapshots cannot guarantee recovery. Do not alter snapshots or retry removal to chase a projection.";

#[cfg_attr(
    not(target_os = "macos"),
    expect(
        clippy::unused_async,
        reason = "The MCP handler uses the same async API as macOS's bounded command"
    )
)]
pub async fn inspect(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("Supply an absolute path on the volume to inspect.".into());
    }
    #[cfg(target_os = "macos")]
    {
        use std::time::Duration;
        let mount = disktree_core::space::volume_root_for(path)
            .ok_or("Cannot identify the path's volume; retention is unknown")?;
        let mut command = tokio::process::Command::new("/usr/bin/tmutil");
        command
            .arg("listlocalsnapshots")
            .arg(&mount)
            .env("LC_ALL", "C")
            .kill_on_drop(true);
        let result =
            tokio::time::timeout(Duration::from_secs(3), command.output())
                .await;
        let count = match result {
            Ok(Ok(output)) if output.status.success() => {
                snapshot_count(&output.stdout)
            }
            _ => None,
        };
        Ok(format!("Read-only Time Machine preflight for {}: {}. {LIMITS}",
            mount.display(), count.map_or_else(
                || "unavailable (command failed, timed out or returned unrecognized output)".into(),
                |count| format!("{count} local Time Machine snapshot(s) listed"),
            )))
    }
    #[cfg(not(target_os = "macos"))]
    Ok(format!(
        "Automatic retention inspection is unavailable on this system. Read the system skill reference for read-only checks. {LIMITS}"
    ))
}

/// Unknown output must not become a measured zero.
#[cfg(any(target_os = "macos", test))]
fn snapshot_count(output: &[u8]) -> Option<usize> {
    if output.len() > 65_536 {
        return None;
    }
    let text = std::str::from_utf8(output).ok()?;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header = lines.next()?;
    if !(header.starts_with("Snapshots for disk ")
        || header.starts_with("Snapshots for volume "))
        || !header.ends_with(':')
    {
        return None;
    }
    let mut count = 0;
    for line in lines {
        if !line.starts_with("com.apple.TimeMachine.")
            || line.chars().any(char::is_whitespace)
        {
            return None;
        }
        count += 1;
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_tmutil_disk_header_accepts_empty_and_populated_listings() {
        // Captured during PR review from:
        // LC_ALL=C /usr/bin/tmutil listlocalsnapshots /
        let header = b"Snapshots for disk /:\n";
        assert_eq!(snapshot_count(header), Some(0));

        // Keep the captured header, with invented snapshot names.
        let mut listing = header.to_vec();
        listing.extend_from_slice(
            b"com.apple.TimeMachine.2026-11-03-100000.local\n\
              com.apple.TimeMachine.2026-11-03-110000.local\n",
        );
        assert_eq!(snapshot_count(&listing), Some(2));
    }

    #[test]
    fn snapshot_presence_absence_and_unavailable_stay_distinct() {
        assert_eq!(snapshot_count(b"Snapshots for volume /:\ncom.apple.TimeMachine.2026-11-03-100000.local\ncom.apple.TimeMachine.2026-11-03-110000.local\n"), Some(2));
        assert_eq!(snapshot_count(b"Snapshots for volume /:\n"), Some(0));
        for output in [
            b"".as_slice(),
            b"Operation not permitted",
            b"Snapshots for disk /:\nerror",
            b"Snapshots for volume /:\nerror",
            &[0xff],
        ] {
            assert_eq!(snapshot_count(output), None);
        }
        assert!(LIMITS.contains("unknown"));
        assert!(LIMITS.contains("not proof"));
    }
}
