//! Deploy only the matching embedded scanner, over the existing SSH pipe.

use crate::payload::Archive;
use std::io::{self, BufRead, Read as _, Write};

// The cache is content-addressed, private and atomically published. We never
// overwrite a system helper or trust an unchecked cached executable. The
// ready handshake keeps scan JSON out of the binary transfer's stdin.
pub const COMMAND: &str = r#"set -eu
printf 'DISKTREE_BOOTSTRAP_V1 %s %s\n' "$(uname -s)" "$(uname -m)"
IFS=' ' read -r digest size
case "$digest" in ''|*[!0-9a-f]*) exit 2;; esac
[ "${#digest}" -eq 64 ] || exit 2
case "$size" in ''|*[!0-9]*) exit 2;; esac
[ "$size" -gt 0 ] && [ "$size" -le 67108864 ] || exit 2
umask 077
dir="${XDG_CACHE_HOME:-$HOME/.cache}/disktree/ssh/$digest"
mkdir -p "$dir"
helper="$dir/disktree-cli"
valid() {
  [ -x "$helper" ] || return 1
  actual=$(sha256sum "$helper") || return 1
  [ "${actual%% *}" = "$digest" ]
}
if ! valid; then
  temporary=$(mktemp "$dir/.upload.XXXXXX")
  trap 'rm -f "$temporary"' EXIT HUP INT TERM
  printf 'DISKTREE_UPLOAD_V1\n'
  head -c "$size" > "$temporary"
  [ "$(wc -c < "$temporary")" -eq "$size" ] || exit 3
  actual=$(sha256sum "$temporary")
  [ "${actual%% *}" = "$digest" ] || exit 3
  chmod 700 "$temporary"
  mv -f "$temporary" "$helper"
  trap - EXIT HUP INT TERM
fi
printf 'DISKTREE_READY_V1\n'
exec "$helper" --stdio-v1
"#;

fn control_line(input: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    input.take(1024).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::other("incomplete SSH deployment handshake"));
    }
    Ok(line.trim_end().into())
}

pub fn deploy(
    archive: &Archive,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    deploy_with_progress(archive, input, output, |_| {})
}

pub(crate) fn deploy_with_progress(
    archive: &Archive,
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut progress: impl FnMut(crate::remote_progress::Stage),
) -> io::Result<()> {
    let hello = control_line(input)?;
    progress(crate::remote_progress::Stage::CheckingHelper);
    let parts: Vec<_> = hello.split_whitespace().collect();
    if parts.len() != 3 || parts[0] != "DISKTREE_BOOTSTRAP_V1" {
        return Err(io::Error::other("unexpected SSH deployment handshake"));
    }
    if parts[1] != "Linux" {
        return Err(io::Error::other(format!(
            "embedded scanners support Linux, not {}",
            parts[1],
        )));
    }
    let arch = match parts[2] {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => {
            return Err(io::Error::other(format!(
                "unsupported remote CPU: {other}",
            )));
        }
    };
    let (entry, bytes) = archive.load(arch)?;
    writeln!(output, "{} {}", entry.sha256, entry.length)?;
    output.flush()?;
    let mut state = control_line(input)?;
    if state == "DISKTREE_UPLOAD_V1" {
        progress(crate::remote_progress::Stage::UploadingHelper);
        output.write_all(&bytes)?;
        output.flush()?;
        state = control_line(input)?;
    }
    if state != "DISKTREE_READY_V1" {
        return Err(io::Error::other("remote scanner deployment failed"));
    }
    Ok(())
}
