//! `cargo xtask bundle`: the macOS app, from a release build.
//!
//! Plain Apple tools and nothing to install: `sips` and `iconutil` draw the
//! icon from its SVG, `codesign` signs, `ditto` zips the way Finder does
//! (keeping the signature's extended attributes), and `notarytool` and
//! `stapler` notarize. Everything lands in `target/bundle/`.
//!
//! Signing is ad hoc unless an identity is given. An ad-hoc app runs on the
//! Mac that built it; a downloaded one needs a Developer ID signature and
//! notarization, or Gatekeeper stops it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const APP: &str = "disktree.app";
const EXECUTABLE: &str = "disktree";
/// What the binary is built for at least; `LSMinimumSystemVersion` in
/// `packaging/macos/Info.plist.in` says the same, and GPUI needs 10.15.7.
const DEPLOYMENT_TARGET: &str = "11.0";

/// The iconset `iconutil` expects: each point size at 1x and 2x.
const ICON_SIZES: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

/// What `bundle` was asked to do.
#[derive(Debug, Default)]
struct Options {
    /// A `Developer ID Application: …` identity from the keychain; ad hoc
    /// when absent.
    identity: Option<String>,
    notarize: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sign" => {
                let identity = args
                    .next()
                    .ok_or("--sign needs an identity, e.g. \"Developer ID Application: Name (TEAMID)\"")?;
                // An empty identity is what CI passes when a secret is
                // missing; codesign would fail on it without saying why.
                if identity.trim().is_empty() {
                    return Err("--sign got an empty identity".into());
                }
                options.identity = Some(identity.clone());
            }
            "--notarize" => options.notarize = true,
            other => return Err(format!("unknown bundle option {other}")),
        }
    }
    if options.notarize && options.identity.is_none() {
        return Err("--notarize needs --sign: Apple notarizes Developer ID \
             signatures only"
            .into());
    }
    Ok(options)
}

pub fn bundle(args: &[String]) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("the app bundle is built on macOS".into());
    }
    let options = parse(args)?;
    let root = workspace_root();
    let version = env!("CARGO_PKG_VERSION");
    let out = root.join("target/bundle");
    let app = out.join(APP);

    // The target directory is named rather than inherited: a
    // `CARGO_TARGET_DIR` or `build.target-dir` elsewhere would put the fresh
    // binary there and leave a stale one here to be bundled.
    let target = root.join("target");
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    tool(
        Command::new(cargo)
            .current_dir(&root)
            .args(["build", "--release", "--locked", "-p", "disktree-app"])
            .arg("--target-dir")
            .arg(&target)
            .env("MACOSX_DEPLOYMENT_TARGET", DEPLOYMENT_TARGET),
    )?;

    // Only ever the bundle this task made, never anything else in target/.
    if app.exists() {
        fs::remove_dir_all(&app)
            .map_err(|err| format!("cannot clear {}: {err}", app.display()))?;
    }
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    for dir in [&macos, &resources] {
        fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }

    let binary = macos.join(EXECUTABLE);
    copy(&target.join("release").join(EXECUTABLE), &binary)?;
    // Local symbols only: the backtrace of a panic still names functions.
    tool(Command::new("strip").arg("-x").arg(&binary))?;

    let plist = fs::read_to_string(root.join("packaging/macos/Info.plist.in"))
        .map_err(|err| format!("cannot read the Info.plist template: {err}"))?;
    write(
        &contents.join("Info.plist"),
        &plist.replace("@VERSION@", version),
    )?;
    tool(
        Command::new("plutil")
            .arg("-lint")
            .arg(contents.join("Info.plist")),
    )?;

    icon(
        &root.join("packaging/macos/AppIcon.svg"),
        &out,
        &resources.join("AppIcon.icns"),
    )?;

    sign(&app, options.identity.as_deref())?;

    let zip = out.join(format!(
        "disktree-{version}-{}-macos.zip",
        std::env::consts::ARCH
    ));
    archive(&app, &zip)?;

    if options.notarize {
        notarize(&zip)?;
        // The ticket is stapled to the app, so zip it again: a zip itself
        // cannot carry one.
        tool(Command::new("xcrun").args(["stapler", "staple"]).arg(&app))?;
        archive(&app, &zip)?;
        tool(
            Command::new("spctl")
                .args(["--assess", "--type", "execute", "-vv"])
                .arg(&app),
        )?;
    }

    println!("\n{}\n{}", app.display(), zip.display());
    Ok(())
}

/// `AppIcon.icns` from the SVG, through an iconset of every size.
fn icon(svg: &Path, scratch: &Path, icns: &Path) -> Result<(), String> {
    let iconset = scratch.join("AppIcon.iconset");
    if iconset.exists() {
        fs::remove_dir_all(&iconset).map_err(|err| {
            format!("cannot clear {}: {err}", iconset.display())
        })?;
    }
    fs::create_dir_all(&iconset)
        .map_err(|err| format!("cannot create {}: {err}", iconset.display()))?;
    for (name, pixels) in ICON_SIZES {
        let pixels = pixels.to_string();
        tool(
            Command::new("sips")
                .args(["-s", "format", "png", "-z", &pixels, &pixels])
                .arg(svg)
                .arg("--out")
                .arg(iconset.join(name))
                // sips names every file it writes; the errors still show.
                .stdout(Stdio::null()),
        )?;
    }
    tool(
        Command::new("iconutil")
            .args(["-c", "icns"])
            .arg(&iconset)
            .arg("-o")
            .arg(icns),
    )
}

/// Sign the bundle, then check the signature holds.
///
/// A real identity gets the hardened runtime and a secure timestamp, both of
/// which notarization requires. No entitlements: the app needs none of what
/// the hardened runtime withholds, and the Trash is reached through
/// `NSFileManager`, which asks for no permission.
fn sign(app: &Path, identity: Option<&str>) -> Result<(), String> {
    let mut command = Command::new("codesign");
    command.arg("--force");
    match identity {
        Some(identity) => {
            command
                .args(["--timestamp", "--options", "runtime", "--sign"])
                .arg(identity);
        }
        None => {
            command.args(["--sign", "-"]);
        }
    }
    tool(command.arg(app))?;
    tool(
        Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(app),
    )
}

fn archive(app: &Path, zip: &Path) -> Result<(), String> {
    if zip.exists() {
        fs::remove_file(zip).map_err(|err| {
            format!("cannot replace {}: {err}", zip.display())
        })?;
    }
    tool(
        Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(app)
            .arg(zip),
    )
}

/// Submit `zip` and wait for Apple's verdict.
///
/// Credentials come from the environment, never the command line, so they
/// stay out of shell history and CI logs: `NOTARY_PROFILE`, a keychain
/// profile saved with `xcrun notarytool store-credentials`, or an App Store
/// Connect API key as `NOTARY_KEY` (the `.p8` file), `NOTARY_KEY_ID` and
/// `NOTARY_ISSUER`.
fn notarize(zip: &Path) -> Result<(), String> {
    let mut command = Command::new("xcrun");
    command
        .args(["notarytool", "submit"])
        .arg(zip)
        .arg("--wait");
    if let Ok(profile) = std::env::var("NOTARY_PROFILE") {
        command.arg("--keychain-profile").arg(profile);
    } else {
        let var = |name: &str| {
            std::env::var(name).map_err(|_| {
                format!(
                    "--notarize needs NOTARY_PROFILE, or NOTARY_KEY, \
                     NOTARY_KEY_ID and NOTARY_ISSUER; {name} is not set"
                )
            })
        };
        command
            .arg("--key")
            .arg(var("NOTARY_KEY")?)
            .arg("--key-id")
            .arg(var("NOTARY_KEY_ID")?)
            .arg("--issuer")
            .arg(var("NOTARY_ISSUER")?);
    }
    tool(&mut command)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to).map(drop).map_err(|err| {
        format!("cannot copy {} to {}: {err}", from.display(), to.display())
    })
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))
}

/// Run one tool, inheriting the terminal so its own report is seen.
fn tool(command: &mut Command) -> Result<(), String> {
    let display = format!("{command:?}");
    let status = command
        .stdin(Stdio::null())
        .status()
        .map_err(|err| format!("could not start {display}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{display} failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn signing_is_ad_hoc_unless_an_identity_is_given() {
        let options = parse(&[]).expect("no options");
        assert!(options.identity.is_none() && !options.notarize);
        let options = parse(&args(&["--sign", "Developer ID Application: X"]))
            .expect("an identity");
        assert_eq!(
            options.identity.as_deref(),
            Some("Developer ID Application: X")
        );
    }

    #[test]
    fn notarizing_needs_a_real_signature() {
        let error = parse(&args(&["--notarize"])).expect_err("no identity");
        assert!(error.contains("--sign"), "{error}");
        assert!(
            parse(&args(&["--sign"])).is_err(),
            "an identity is required"
        );
        assert!(
            parse(&args(&["--sign", ""])).is_err(),
            "an empty identity is not one"
        );
    }

    #[test]
    fn the_plist_template_names_the_executable_and_takes_the_version() {
        let plist = include_str!("../../packaging/macos/Info.plist.in");
        assert!(plist.contains(&format!("<string>{EXECUTABLE}</string>")));
        assert!(plist.contains("@VERSION@"));
        assert!(
            plist.contains(&format!("<string>{DEPLOYMENT_TARGET}</string>"))
        );
    }
}
