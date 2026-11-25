//! Static Linux helpers and an appended, verifiable executable trailer.
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("portable packaging currently runs on Linux".into());
    }
    super::run(&["build", "--release", "--locked", "-p", "disktree-app"])?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from);
    let rustc = |args: &[&str]| -> Result<String, String> {
        let output = Command::new("rustc")
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    };
    let sysroot = rustc(&["--print", "sysroot"])?;
    let version = rustc(&["-vV"])?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc did not report a host triple")?;
    let linker = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld");
    let mut helpers = Vec::new();
    for arch in ["x86_64", "aarch64"] {
        let triple = format!("{arch}-unknown-linux-musl");
        let key = format!(
            "CARGO_TARGET_{}_LINKER",
            triple.to_uppercase().replace('-', "_")
        );
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "--locked",
                "-p",
                "disktree-cli",
                "--target",
                &triple,
            ])
            .env(key, &linker)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("static helper build failed; install targets with: \
                 rustup target add x86_64-unknown-linux-musl \
                 aarch64-unknown-linux-musl"
                .into());
        }
        helpers.push((
            arch.into(),
            target.join(triple).join("release/disktree-cli"),
        ));
    }
    let output = target.join("portable/disktree");
    std::fs::create_dir_all(output.parent().unwrap())
        .map_err(|e| e.to_string())?;
    std::fs::copy(target.join("release/disktree"), &output)
        .map_err(|e| e.to_string())?;
    // strip rewrites ELF and discards overlays, so it must precede embedding.
    let status = Command::new("strip")
        .arg(&output)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("strip failed".into());
    }
    disktree_core::payload::append(&output, &helpers)
        .map_err(|e| e.to_string())?;
    println!("Portable app: {}", output.display());
    Ok(())
}
