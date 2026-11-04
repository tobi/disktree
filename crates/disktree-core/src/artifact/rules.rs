//! Structural rules ported from `DiskHound`. See licenses/bundled.txt.

use super::Artifact;

pub(super) fn numeric_version(
    value: &str,
    min_parts: usize,
    max_parts: usize,
) -> bool {
    let mut count = 0;
    for part in value.split('.') {
        count += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    (min_parts..=max_parts).contains(&count)
}

fn dotnet_framework(value: &str) -> bool {
    let (framework, platform) = value
        .split_once('-')
        .map_or((value, None), |(a, b)| (a, Some(b)));
    let Some(version) = framework.strip_prefix("net") else {
        return false;
    };
    let valid = if let Some(version) = version
        .strip_prefix("standard")
        .or_else(|| version.strip_prefix("coreapp"))
    {
        numeric_version(version, 2, 2)
    } else {
        numeric_version(version, 1, 2)
    };
    valid
        && platform.is_none_or(|platform| {
            let letters =
                platform.bytes().take_while(u8::is_ascii_lowercase).count();
            matches!(
                &platform[..letters],
                "windows"
                    | "android"
                    | "ios"
                    | "macos"
                    | "maccatalyst"
                    | "tvos"
                    | "browser"
            ) && (letters == platform.len()
                || numeric_version(&platform[letters..], 1, usize::MAX))
        })
}

fn go_module_version(name: &str) -> bool {
    let Some((module, version)) = name.split_once("@v") else {
        return false;
    };
    if module.is_empty()
        || !module
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!_-".contains(&b))
    {
        return false;
    }
    let suffix = version.find(['-', '+']);
    let base = suffix.map_or(version, |i| &version[..i]);
    numeric_version(base, 3, 3)
        && suffix.is_none_or(|i| {
            let suffix = &version[i + 1..];
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
        })
}

fn cache_tool_kind(tool: &str) -> Option<Artifact> {
    match tool {
        "pip" | "uv" => Some(Artifact::Python),
        "go-build" => Some(Artifact::GoModule),
        "zig" => Some(Artifact::CompilerCache),
        "coursier" => Some(Artifact::Jvm),
        "ccache" | "sccache" | "mozilla.sccache" | "vscode-cpptools" => {
            Some(Artifact::CompilerCache)
        }
        "composer" | "yarn" | "pnpm" | "electron" | "electron-builder"
        | "ms-playwright" | "cypress" | "homebrew" | "puppeteer"
        | "node-gyp" | "org.swift.swiftpm" => Some(Artifact::PackageCache),
        _ => None,
    }
}

// Fixed-width structural checks, matching TypeScript. No filesystem probes.
fn language_layout(
    parts: &[&str],
    i: usize,
    lower: &str,
) -> Option<(usize, Artifact)> {
    if !matches!(
        lower,
        "library"
            | "appdata"
            | ".cache"
            | ".pub-cache"
            | ".dart_tool"
            | ".composer"
            | ".gem"
            | "vendor"
            | ".hex"
            | "_build"
            | ".stack-work"
            | "dist-newstyle"
    ) {
        return None;
    }
    let at = |offset: usize, value: &str| {
        parts
            .get(i + offset)
            .is_some_and(|s| s.eq_ignore_ascii_case(value))
    };
    let next = parts
        .get(i + 1)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if lower == "library"
        && next == "developer"
        && at(2, "xcode")
        && at(3, "deriveddata")
    {
        return Some((i + 4, Artifact::CompilerCache));
    }
    if matches!(
        (lower, next.as_str()),
        (".pub-cache", "hosted")
            | (".composer", "cache")
            | (".gem", "specs")
            | (".hex", "packages")
    ) {
        return Some((i + 2, Artifact::PackageCache));
    }
    if lower == ".pub-cache" && next == "git" && at(2, "cache") {
        return Some((i + 3, Artifact::PackageCache));
    }
    if lower == "appdata"
        && next == "local"
        && at(2, "pub")
        && at(3, "cache")
        && at(4, "hosted")
    {
        return Some((i + 5, Artifact::PackageCache));
    }
    if lower == "appdata"
        && next == "local"
        && at(2, "composer")
        && (at(3, "files") || at(3, "repo") || at(3, "vcs"))
    {
        return Some((i + 4, Artifact::PackageCache));
    }
    if lower == ".cache" && next == "gem" && (at(2, "specs") || at(2, "gems")) {
        return Some((i + 3, Artifact::PackageCache));
    }
    let ruby = if lower == ".gem" && next == "ruby" {
        Some(i + 1)
    } else if lower == "vendor" && next == "bundle" && at(2, "ruby") {
        Some(i + 2)
    } else {
        None
    };
    if let Some(ruby) = ruby
        && parts
            .get(ruby + 1)
            .is_some_and(|s| numeric_version(s, 3, 3))
        && parts
            .get(ruby + 2)
            .is_some_and(|s| s.eq_ignore_ascii_case("cache"))
    {
        return Some((ruby + 3, Artifact::PackageCache));
    }
    if matches!(
        (lower, next.as_str()),
        (".dart_tool", "flutter_build")
            | (".stack-work", "dist")
            | ("dist-newstyle", "build" | "cache")
    ) {
        return Some((i + 2, Artifact::CompilerCache));
    }
    if lower == "_build"
        && matches!(next.as_str(), "dev" | "test" | "prod" | "shared")
        && at(2, "lib")
        && at(4, "ebin")
        && parts.get(i + 3).is_some_and(|name| {
            name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    {
        return Some((i + 5, Artifact::CompilerCache));
    }
    None
}

pub(super) fn classify_parts(
    parts: &[&str],
    start: usize,
) -> Option<(usize, Artifact)> {
    for i in start..parts.len() {
        super::step();
        let lower = parts[i].to_ascii_lowercase();
        if let Some((end, kind)) = language_layout(parts, i, &lower) {
            return Some((end, kind));
        }
        if matches!(
            lower.as_str(),
            ".cache" | "library" | ".local" | "appdata" | ".npm"
        ) {
            let next = parts
                .get(i + 1)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let after = parts
                .get(i + 2)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let cache_tool = if lower == ".cache" {
                Some(i + 1)
            } else if lower == "library" && next == "caches" {
                Some(i + 2)
            } else {
                None
            };
            if let Some(tool) = cache_tool
                && let Some(kind) = parts
                    .get(tool)
                    .and_then(|s| cache_tool_kind(&s.to_ascii_lowercase()))
            {
                return Some((tool + 1, kind));
            }
            if ((lower == ".local" && next == "share")
                || (lower == "appdata" && next == "local"))
                && after == "nuget"
                && parts.get(i + 3).is_some_and(|s| {
                    matches!(
                        s.to_ascii_lowercase().as_str(),
                        "http-cache" | "v3-cache" | "plugins-cache"
                    )
                })
            {
                return Some((i + 4, Artifact::Dotnet));
            }
            if lower == ".npm" {
                if next == "_cacache" {
                    return Some((i + 2, Artifact::PackageCache));
                }
                continue;
            }
        }
        if matches!(
            lower.as_str(),
            ".yarn"
                | ".bun"
                | ".gradle"
                | ".m2"
                | ".nuget"
                | ".vercel"
                | ".netlify"
                | ".output"
                | ".venv"
                | "venv"
                | ".tox"
                | "obj"
        ) {
            let next = parts
                .get(i + 1)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let after = parts
                .get(i + 2)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            // Only generated/cache children, not whole tool homes.
            match lower.as_str() {
                ".yarn" => {
                    if next == "berry" && after == "cache" {
                        return Some((i + 3, Artifact::PackageCache));
                    }
                    return matches!(next.as_str(), "cache" | "unplugged")
                        .then(|| (i + 2, Artifact::PackageCache));
                }
                ".bun" => {
                    return (next == "install" && after == "cache")
                        .then(|| (i + 3, Artifact::PackageCache));
                }
                ".gradle" => {
                    if next == "caches" || numeric_version(&next, 2, 3) {
                        return Some((i + 2, Artifact::Jvm));
                    }
                    return (next == "wrapper" && after == "dists")
                        .then(|| (i + 3, Artifact::Jvm));
                }
                ".m2" => {
                    return (next == "repository")
                        .then(|| (i + 2, Artifact::Jvm));
                }
                ".nuget" => {
                    return (next == "packages")
                        .then(|| (i + 2, Artifact::Dotnet));
                }
                ".vercel" | ".netlify" | ".output" => {
                    let output = match lower.as_str() {
                        ".vercel" => {
                            matches!(next.as_str(), "cache" | "output")
                        }
                        ".netlify" => next == "cache",
                        _ => matches!(next.as_str(), "public" | "server"),
                    };
                    return output.then(|| (i + 2, Artifact::JsBuild));
                }
                ".venv" | "venv" | ".tox" => {
                    let lib = i + if lower == ".tox" { 2 } else { 1 };
                    if parts
                        .get(lib)
                        .is_some_and(|s| s.eq_ignore_ascii_case("lib"))
                    {
                        let version = parts
                            .get(lib + 1)
                            .map(|s| s.to_ascii_lowercase())
                            .unwrap_or_default();
                        let packages = if version == "site-packages" {
                            Some(lib + 1)
                        } else if version.strip_prefix("python").is_some_and(
                            |v| {
                                numeric_version(
                                    v.strip_suffix('t').unwrap_or(v),
                                    2,
                                    2,
                                )
                            },
                        ) {
                            Some(lib + 2)
                        } else {
                            None
                        };
                        if let Some(packages) = packages
                            && parts.get(packages).is_some_and(|s| {
                                s.eq_ignore_ascii_case("site-packages")
                            })
                        {
                            return Some((packages + 1, Artifact::Python));
                        }
                    }
                }
                "obj"
                    if matches!(next.as_str(), "debug" | "release")
                        && dotnet_framework(&after) =>
                {
                    return Some((i + 3, Artifact::Dotnet));
                }
                _ => {}
            }
            continue;
        }
        if matches!(lower.as_str(), "ccache" | "sccache" | "mozilla.sccache") {
            let parent = i.checked_sub(1).and_then(|j| parts.get(j));
            let grandparent = i.checked_sub(2).and_then(|j| parts.get(j));
            if parent.zip(grandparent).is_some_and(|(p, gp)| {
                (p.eq_ignore_ascii_case("caches")
                    && gp.eq_ignore_ascii_case("library"))
                    || (p.eq_ignore_ascii_case("local")
                        && gp.eq_ignore_ascii_case("appdata"))
                    || (lower == "sccache"
                        && p.eq_ignore_ascii_case("mozilla")
                        && gp.eq_ignore_ascii_case("local")
                        && i.checked_sub(3)
                            .and_then(|j| parts.get(j))
                            .is_some_and(|s| s.eq_ignore_ascii_case("appdata")))
            }) {
                return Some((i + 1, Artifact::CompilerCache));
            }
            continue;
        }
        if lower == "target" {
            // Match only tool-specific subtrees, never a whole ambiguous
            // target/profile tree or an ancestor's project language.
            let next = parts
                .get(i + 1)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let scala = next.strip_prefix("scala-").is_some_and(|version| {
                let mut components = version.split('.');
                let major = components.next().unwrap_or_default();
                let rest: Vec<_> = components.collect();
                (major == "3" || (major == "2" && !rest.is_empty()))
                    && rest.iter().all(|s| {
                        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
                    })
            });
            if scala
                || matches!(next.as_str(), "maven-status" | "maven-archiver")
            {
                return Some((i + 2, Artifact::Jvm));
            }
            let triple = next.split('-').count() >= 3
                && next.split('-').take(2).all(|s| !s.is_empty())
                && next.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'_'
                        || b == b'-'
                })
                && next.splitn(3, '-').nth(2).is_some_and(|s| {
                    s.as_bytes().first().is_some_and(|b| {
                        b.is_ascii_alphanumeric() || *b == b'_'
                    })
                });
            let profile = if matches!(next.as_str(), "debug" | "release") {
                i + 1
            } else if triple {
                i + 2
            } else {
                continue;
            };
            if parts.get(profile).is_some_and(|s| {
                s.eq_ignore_ascii_case("debug")
                    || s.eq_ignore_ascii_case("release")
            }) && parts.get(profile + 1).is_some_and(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    "deps" | "incremental" | ".fingerprint"
                )
            }) {
                return Some((profile + 2, Artifact::RustTarget));
            }
            if parts.get(profile).is_some_and(|s| {
                s.eq_ignore_ascii_case("debug")
                    || s.eq_ignore_ascii_case("release")
            }) && parts
                .get(profile + 1)
                .is_some_and(|s| s.eq_ignore_ascii_case("build"))
                && parts.get(profile + 2).is_some_and(|s| {
                    s.rsplit_once('-').is_some_and(|(name, hash)| {
                        !name.is_empty()
                            && (name.as_bytes()[0].is_ascii_alphanumeric()
                                || name.as_bytes()[0] == b'_')
                            && name.bytes().all(|b| {
                                b.is_ascii_alphanumeric()
                                    || b == b'_'
                                    || b == b'-'
                            })
                            && hash.len() == 16
                            && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    })
                })
            {
                return Some((profile + 3, Artifact::RustTarget));
            }
            continue;
        }
        if lower == ".cargo"
            && i + 1 < parts.len()
            && parts[i + 1].eq_ignore_ascii_case("registry")
        {
            return Some((i + 2, Artifact::CargoRegistry));
        }
        if lower == "pkg"
            && parts
                .get(i + 1)
                .is_some_and(|s| s.eq_ignore_ascii_case("mod"))
        {
            if parts
                .get(i + 2)
                .is_some_and(|s| s.eq_ignore_ascii_case("cache"))
                && parts
                    .get(i + 3)
                    .is_some_and(|s| s.eq_ignore_ascii_case("download"))
            {
                return Some((i + 4, Artifact::GoModule));
            }
            // Same bounded layout search as ARTIFACT_ROOT_LOOKBACK in TS.
            if parts.get(i + 2).is_some_and(|s| s.contains('.')) {
                for (end, part) in
                    parts.iter().enumerate().take(i + 6).skip(i + 2)
                {
                    super::step();
                    if go_module_version(&part.to_ascii_lowercase()) {
                        return Some((end + 1, Artifact::GoModule));
                    }
                }
            }
            continue;
        }
        // pnpm's global store outside a `.pnpm-store` folder:
        // `$PNPM_HOME/store`. Same rule as `classifyArtifactPath` in
        // src/shared/devArtifacts.ts.
        if lower == "pnpm"
            && i + 1 < parts.len()
            && parts[i + 1].eq_ignore_ascii_case("store")
        {
            return Some((i + 2, Artifact::PackageCache));
        }
        if lower == ".cache" && i + 1 < parts.len() {
            let next = parts[i + 1].to_ascii_lowercase();
            if matches!(next.as_str(), "ccache" | "sccache" | "yarn" | "pnpm") {
                let kind = if next == "yarn" || next == "pnpm" {
                    Artifact::PackageCache
                } else {
                    Artifact::CompilerCache
                };
                return Some((i + 2, kind));
            }
        }
        // Only the provider downloads, which `terraform init` puts back
        // from the lock file. The rest of .terraform records the selected
        // workspace and the last backend config, so it stays.
        if lower == ".terraform" && i + 1 < parts.len() {
            let next = parts[i + 1].to_ascii_lowercase();
            if matches!(next.as_str(), "providers" | "plugins") {
                return Some((i + 2, Artifact::Terraform));
            }
        }
        // The documented plugin_cache_dir. `.terraform.d/plugins` holds
        // providers installed by hand, so it stays.
        if lower == ".terraform.d"
            && i + 1 < parts.len()
            && parts[i + 1].eq_ignore_ascii_case("plugin-cache")
        {
            return Some((i + 2, Artifact::Terraform));
        }
        // The repo's history, not its working files. Every file here is
        // below a `.git` folder; a linked worktree's or submodule's `.git`
        // is a file and ends the path, so it never matches.
        if lower == ".git" && i + 1 < parts.len() {
            return Some((i + 1, Artifact::GitRepo));
        }
        if let Some(kind) = mapped_kind(&lower) {
            let depth =
                if matches!(kind, Artifact::Worktree) && i + 1 < parts.len() {
                    i + 2
                } else {
                    i + 1
                };
            return Some((depth, kind));
        }
    }
    None
}

fn mapped_kind(lower: &str) -> Option<Artifact> {
    Some(match lower {
        "node_modules" => Artifact::NodeModules,
        ".pnpm-store" => Artifact::PackageCache,
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" => {
            Artifact::JsBuild
        }
        "__pycache__" | ".mypy_cache" | ".pytest_cache" | ".ruff_cache" => {
            Artifact::Python
        }
        "cmakefiles" => Artifact::CmakeBuild,
        ".zig-cache" => Artifact::CompilerCache,
        ".worktrees" => Artifact::Worktree,
        "diagoutputdir" | "rdclientautotrace" => Artifact::DiagLogs,
        _ => return None,
    })
}
